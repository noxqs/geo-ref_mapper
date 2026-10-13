//! Rasterises the app mark into an `IconData` so the taskbar / window manager
//! entry carries the same glyph the title bar paints.

use eframe::egui::IconData;

/// The size the window icon is handed over at.
const SIZE: usize = 128;
const SAMPLES: usize = 3; // Per axis, so 9 samples per pixel.

const BG: [f32; 3] = [0.357, 0.549, 1.0]; // theme::ACCENT
const MARK: [f32; 3] = [0.063, 0.067, 0.078]; // theme::WINDOW_BG

/// Points describing the ridge inside the photo frame, in unit coordinates.
const RIDGE: [[f32; 2]; 5] = [
    [0.245, 0.705],
    [0.420, 0.445],
    [0.530, 0.580],
    [0.625, 0.470],
    [0.755, 0.705],
];

pub fn icon() -> IconData {
    IconData {
        rgba: rgba(SIZE),
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// The mark as straight (unmultiplied) RGBA, `size` pixels square.
///
/// Drawn at whatever size is asked for rather than scaled from one bitmap: the
/// icon theme wants 48, 64, 128 and 256, and a 48 px downscale of the 128 px
/// version loses the frame outline, which is a single pixel wide there.
pub fn rgba(size: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(size * size * 4);

    for y in 0..size {
        for x in 0..size {
            let (mut bg_cov, mut mark_cov) = (0.0_f32, 0.0_f32);

            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let u = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    if in_rounded_rect(u, v, 0.02, 0.98, 0.02, 0.98, 0.22) {
                        bg_cov += 1.0;
                    }
                    if in_mark(u, v) {
                        mark_cov += 1.0;
                    }
                }
            }

            let total = (SAMPLES * SAMPLES) as f32;
            let bg_alpha = bg_cov / total;
            let mark_alpha = (mark_cov / total).min(bg_alpha);

            // Mark over background, background over transparency.
            let mut out = [0.0_f32; 3];
            for c in 0..3 {
                out[c] = BG[c] * (bg_alpha - mark_alpha) + MARK[c] * mark_alpha;
            }

            let alpha = bg_alpha;
            for c in 0..3 {
                // Un-premultiply: IconData expects straight (unmultiplied) RGBA.
                let value = if alpha > 0.0 { out[c] / alpha } else { 0.0 };
                rgba.push((value.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
            rgba.push((alpha.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }

    rgba
}

fn in_mark(u: f32, v: f32) -> bool {
    // Photo frame outline: inside the outer rounded rect but outside the inner one.
    let outer = in_rounded_rect(u, v, 0.20, 0.80, 0.22, 0.78, 0.07);
    let inner = in_rounded_rect(u, v, 0.245, 0.755, 0.265, 0.735, 0.045);
    if outer && !inner {
        return true;
    }
    if !inner {
        return false;
    }

    // Sun.
    let (dx, dy) = (u - 0.365, v - 0.370);
    if dx * dx + dy * dy <= 0.058 * 0.058 {
        return true;
    }

    in_polygon(u, v, &RIDGE)
}

fn in_rounded_rect(u: f32, v: f32, x0: f32, x1: f32, y0: f32, y1: f32, radius: f32) -> bool {
    if u < x0 || u > x1 || v < y0 || v > y1 {
        return false;
    }
    // Distance from the inner rectangle the corner circles are centred on.
    let cx = u.clamp(x0 + radius, x1 - radius);
    let cy = v.clamp(y0 + radius, y1 - radius);
    let (dx, dy) = (u - cx, v - cy);
    dx * dx + dy * dy <= radius * radius
}

fn in_polygon(u: f32, v: f32, points: &[[f32; 2]]) -> bool {
    // The ridge is an open outline closed along its baseline; treat it as a
    // filled polygon and use an even-odd crossing test.
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (xi, yi) = (points[i][0], points[i][1]);
        let (xj, yj) = (points[j][0], points[j][1]);
        if (yi > v) != (yj > v) {
            let t = (v - yi) / (yj - yi);
            if u < xi + t * (xj - xi) {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}
