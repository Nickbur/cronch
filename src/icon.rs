//! Tray icon derived from the bundled brand asset (`assets/tray.png`).
//! Active = full colour; paused = desaturated so the state reads at a glance.

/// Brand tray glyph (transparent background), embedded at compile time.
static TRAY_PNG: &[u8] = include_bytes!("../assets/tray.png");

pub fn make_icon(active: bool) -> tray_icon::Icon {
    let img = image::load_from_memory(TRAY_PNG).expect("valid tray png");
    let (w, h) = (img.width(), img.height());
    let mut rgba = img.into_rgba8().into_raw();

    if !active {
        // Blend mostly toward luma (keep a hint of colour) while preserving
        // the alpha channel, so paused looks muted rather than a flat block.
        for px in rgba.chunks_exact_mut(4) {
            let luma =
                (0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32) as u16;
            px[0] = (px[0] as u16 / 5 + luma * 4 / 5) as u8;
            px[1] = (px[1] as u16 / 5 + luma * 4 / 5) as u8;
            px[2] = (px[2] as u16 / 5 + luma * 4 / 5) as u8;
        }
    }

    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid tray icon")
}
