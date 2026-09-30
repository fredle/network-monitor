//! Regenerates the icon assets from the procedural drawing in `src/icon.rs`:
//!   cargo run --example gen_assets
//! Writes `assets/app.ico` and the Microsoft Store logo set in `assets/store`.

#[path = "../src/model.rs"]
#[allow(dead_code)]
mod model;
#[path = "../src/settings.rs"]
#[allow(dead_code)]
mod settings;
#[path = "../src/icon.rs"]
#[allow(dead_code)]
mod icon;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::ExtendedColorType;
use std::path::Path;

fn png(path: &Path, w: u32, h: u32, rgba: Vec<u8>) {
    image::save_buffer(path, &rgba, w, h, ExtendedColorType::Rgba8).unwrap();
}

/// Centre an app-icon square on a transparent w x h canvas.
fn padded(w: u32, h: u32) -> Vec<u8> {
    let side = (w.min(h) as f32 * 0.8) as u32;
    let sq = icon::app_rgba(side.max(16));
    let mut out = vec![0u8; (w * h * 4) as usize];
    let (ox, oy) = ((w - side) / 2, (h - side) / 2);
    for y in 0..side {
        for x in 0..side {
            let s = ((y * side + x) * 4) as usize;
            let d = (((y + oy) * w + x + ox) * 4) as usize;
            out[d..d + 4].copy_from_slice(&sq[s..s + 4]);
        }
    }
    out
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(root.join("assets")).unwrap();

    let frames: Vec<IcoFrame> = [16u32, 24, 32, 48, 64, 128, 256]
        .iter()
        .map(|&s| IcoFrame::as_png(&icon::app_rgba(s), s, s, ExtendedColorType::Rgba8).unwrap())
        .collect();
    let f = std::fs::File::create(root.join("assets/app.ico")).unwrap();
    IcoEncoder::new(f).encode_images(&frames).unwrap();

    let assets = root.join("assets/store");
    std::fs::create_dir_all(&assets).unwrap();
    for (name, w, h) in [
        ("StoreLogo.png", 50, 50),
        ("Square44x44Logo.png", 44, 44),
        ("Square71x71Logo.png", 71, 71),
        ("Square150x150Logo.png", 150, 150),
        ("Square310x310Logo.png", 310, 310),
        ("Wide310x150Logo.png", 310, 150),
    ] {
        png(&assets.join(name), w, h, padded(w, h));
    }
    println!("wrote assets/app.ico and {} Store logos", 6);
}
