//! Pinned 256-color palette table and RGB → palette-index downsampling.
//!
//! Identical to tui-p2-contrast (issue #58). Duplicated for self-containment.

use crate::color::Rgb;

const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];
const GRAY_VALUES: [u8; 24] = gray_values();

const fn gray_values() -> [u8; 24] {
    let mut out = [0u8; 24];
    let mut i = 0u8;
    while i < 24 {
        out[i as usize] = 8 + i * 10;
        i += 1;
    }
    out
}

#[must_use]
pub fn palette_rgb(index: u8) -> Rgb {
    match index {
        0 => Rgb(0, 0, 0),
        1 => Rgb(205, 0, 0),
        2 => Rgb(0, 205, 0),
        3 => Rgb(205, 205, 0),
        4 => Rgb(0, 0, 238),
        5 => Rgb(205, 0, 205),
        6 => Rgb(0, 205, 205),
        7 => Rgb(229, 229, 229),
        8 => Rgb(127, 127, 127),
        9 => Rgb(255, 0, 0),
        10 => Rgb(0, 255, 0),
        11 => Rgb(255, 255, 0),
        12 => Rgb(92, 92, 255),
        13 => Rgb(255, 0, 255),
        14 => Rgb(0, 255, 255),
        15 => Rgb(255, 255, 255),
        16..=231 => {
            let i = index - 16;
            let r = CUBE_VALUES[(i / 36) as usize];
            let g = CUBE_VALUES[((i % 36) / 6) as usize];
            let b = CUBE_VALUES[(i % 6) as usize];
            Rgb(r, g, b)
        }
        232..=255 => {
            let v = GRAY_VALUES[(index - 232) as usize];
            Rgb(v, v, v)
        }
    }
}

fn color_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = i32::from(r1) - i32::from(r2);
    let dg = i32::from(g1) - i32::from(g2);
    let db = i32::from(b1) - i32::from(b2);
    let weighted = (dr * dr * 299 + dg * dg * 587 + db * db * 114) / 1000;
    weighted.try_into().unwrap_or(u32::MAX)
}

fn closest_cube(value: u8) -> usize {
    let mut best = 0usize;
    let mut best_dist = u32::MAX;
    for (i, c) in CUBE_VALUES.iter().enumerate() {
        let d = (i32::from(value) - i32::from(*c)).unsigned_abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

fn closest_gray(gray: u8) -> usize {
    let mut best = 0usize;
    let mut best_dist = u32::MAX;
    for (i, v) in GRAY_VALUES.iter().enumerate() {
        let d = (i32::from(gray) - i32::from(*v)).unsigned_abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

#[must_use]
pub fn rgb_to_256(rgb: Rgb) -> u8 {
    let Rgb(r, g, b) = rgb;
    let r_idx = closest_cube(r);
    let g_idx = closest_cube(g);
    let b_idx = closest_cube(b);
    let cube_r = CUBE_VALUES[r_idx];
    let cube_g = CUBE_VALUES[g_idx];
    let cube_b = CUBE_VALUES[b_idx];
    let cube_index = 16 + 36 * r_idx + 6 * g_idx + b_idx;
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);
    let gray = u8::try_from((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000)
        .unwrap_or(255);
    let gray_idx = closest_gray(gray);
    let gray_value = GRAY_VALUES[gray_idx];
    let gray_index = 232 + gray_idx;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);
    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = u32::from(max_c) - u32::from(min_c);
    if spread < 10 && gray_dist < cube_dist {
        u8::try_from(gray_index).unwrap_or(255)
    } else {
        u8::try_from(cube_index).unwrap_or(255)
    }
}

#[must_use]
pub fn downsample_256(rgb: Rgb) -> Rgb {
    palette_rgb(rgb_to_256(rgb))
}
