//! Procedurally-drawn tray icons (no external asset needed).
//! Active = brand teal, paused = grey. A simple clock motif.

const SIZE: u32 = 32;

fn put(buf: &mut [u8], x: i32, y: i32, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
        return;
    }
    let idx = ((y as u32 * SIZE + x as u32) * 4) as usize;
    buf[idx] = rgba[0];
    buf[idx + 1] = rgba[1];
    buf[idx + 2] = rgba[2];
    buf[idx + 3] = rgba[3];
}

fn draw_line(buf: &mut [u8], mut x0: i32, mut y0: i32, x1: i32, y1: i32, rgba: [u8; 4]) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put(buf, x0, y0, rgba);
        // thicken slightly
        put(buf, x0 + 1, y0, rgba);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

pub fn make_icon(active: bool) -> tray_icon::Icon {
    let (r, g, b) = if active {
        (45u8, 191u8, 168u8)
    } else {
        (140u8, 140u8, 150u8)
    };
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    let cx = 15.5f32;
    let cy = 15.5f32;
    let radius = 14.5f32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= radius {
                put(&mut rgba, x as i32, y as i32, [r, g, b, 255]);
            }
        }
    }

    // Clock hands.
    let white = [255u8, 255u8, 255u8, 255u8];
    draw_line(&mut rgba, 16, 16, 16, 7, white); // minute hand (up)
    draw_line(&mut rgba, 16, 16, 22, 16, white); // hour hand (right)

    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid tray icon")
}
