//! Procedurally drawn icons (no image assets to ship): the coloured status dot
//! for the tray and the Wi-Fi-arcs app icon for windows and the .exe.
//! Shapes are analytic signed-distance fields, so edges are anti-aliased at any size.

use crate::model::Health;

type Px = [f32; 4]; // premultiplied-free straight RGBA 0..1

fn over(dst: &mut Px, rgb: [f32; 3], a: f32) {
    let a = a.clamp(0.0, 1.0);
    let out_a = a + dst[3] * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        dst[i] = (rgb[i] * a + dst[i] * dst[3] * (1.0 - a)) / out_a;
    }
    dst[3] = out_a;
}

fn to_bytes(px: &[Px]) -> Vec<u8> {
    px.iter().flat_map(|p| p.iter().map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

/// Coverage (0..1) for a shape whose signed distance at this pixel is `d` pixels.
fn cov(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

pub fn status_color(h: Health) -> [f32; 3] {
    match h {
        Health::Good => rgb(40, 180, 80),
        Health::Warn => rgb(220, 160, 30),
        Health::Bad => rgb(210, 50, 50),
        Health::Roaming => rgb(80, 130, 230),
        Health::Idle => rgb(130, 130, 130),
    }
}

/// Round dot with a dark rim, `size` x `size` RGBA8.
pub fn status_rgba(h: Health, size: u32) -> Vec<u8> {
    let s = size as f32;
    let (c, r) = (s / 2.0, s * 0.47);
    let fill = status_color(h);
    let rim = rgb(30, 30, 30);
    let mut px = vec![[0.0f32; 4]; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt() - r;
            let p = &mut px[(y * size + x) as usize];
            over(p, rim, cov(d));
            over(p, fill, cov(d + (s / 16.0).max(1.0)));
        }
    }
    to_bytes(&px)
}

/// Dark rounded square with three green Wi-Fi arcs and a dot.
pub fn app_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let bg = rgb(24, 26, 32);
    let green = rgb(60, 200, 100);
    let mut px = vec![[0.0f32; 4]; (size * size) as usize];
    let (cx, cy) = (s * 0.5, s * 0.78);
    let corner = s * 0.22;
    let stroke = s * 0.078;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let p = &mut px[(y * size + x) as usize];

            // rounded square
            let (qx, qy) = ((fx - s / 2.0).abs() - (s / 2.0 - corner), (fy - s / 2.0).abs() - (s / 2.0 - corner));
            let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - corner;
            over(p, bg, cov(d));

            // arcs: annulus segments pointing up (225..315 degrees), rounded ends approximated
            let (dx, dy) = (fx - cx, fy - cy);
            let dist = (dx * dx + dy * dy).sqrt();
            let ang = dy.atan2(dx).to_degrees(); // -180..180, up is negative
            for r in [0.19, 0.33, 0.47] {
                let ring = (dist - r * s).abs() - stroke / 2.0;
                let within = (-135.0..=-45.0).contains(&ang);
                if within {
                    over(p, green, cov(ring));
                } else {
                    // round caps at the arc ends
                    for end in [-135.0f32, -45.0] {
                        let (ex, ey) = (cx + r * s * end.to_radians().cos(), cy + r * s * end.to_radians().sin());
                        let cd = ((fx - ex).powi(2) + (fy - ey).powi(2)).sqrt() - stroke / 2.0;
                        over(p, green, cov(cd));
                    }
                }
            }
            over(p, green, cov(dist - s * 0.06));
        }
    }
    to_bytes(&px)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_icon_has_opaque_centre_and_transparent_corner() {
        let b = status_rgba(Health::Good, 32);
        let at = |x: usize, y: usize| &b[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(at(16, 16)[3], 255);
        assert_eq!(at(0, 0)[3], 0);
        // Green channel dominates for "good".
        assert!(at(16, 16)[1] > at(16, 16)[0]);
    }

    #[test]
    fn every_health_state_has_a_distinct_colour() {
        let all = [Health::Idle, Health::Good, Health::Warn, Health::Bad, Health::Roaming];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(status_color(*a), status_color(*b));
            }
        }
    }

    #[test]
    fn app_icon_has_expected_size_and_content() {
        let b = app_rgba(64);
        assert_eq!(b.len(), 64 * 64 * 4);
        assert!(b.chunks(4).any(|p| p[1] > 150 && p[0] < 100), "green arcs should be drawn");
        assert_eq!(b[3], 0, "corner pixel is outside the rounded square");
    }
}
