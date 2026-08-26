//! Pinned 256-color palette table and RGB → palette-index downsampling.
//!
//! The palette is the standard xterm 256-color table:
//! - 0–15: standard ANSI 16 (not produced by `rgb_to_256`, but defined for
//!   completeness)
//! - 16–231: 6×6×6 color cube, index = 16 + 36·r + 6·g + b
//! - 232–255: 24-step grayscale ramp, value = 8 + 10·i

use crate::color::Rgb;

/// Cube component values for the 6×6×6 color cube (indices 16–231).
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Grayscale ramp values for indices 232–255.
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

/// Look up the RGB value for a 256-color palette index.
///
/// Indices 0–15 return the xterm standard ANSI 16 colors. Indices 16–231
/// return the cube color. Indices 232–255 return the grayscale ramp color.
/// Out-of-range indices return black.
#[must_use]
pub fn palette_rgb(index: u8) -> Rgb {
    match index {
        // Standard ANSI 16 (xterm defaults)
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
        // 6×6×6 color cube
        16..=231 => {
            let i = index - 16;
            let r = CUBE_VALUES[(i / 36) as usize];
            let g = CUBE_VALUES[((i % 36) / 6) as usize];
            let b = CUBE_VALUES[(i % 6) as usize];
            Rgb(r, g, b)
        }
        // 24-step grayscale ramp
        232..=255 => {
            let v = GRAY_VALUES[(index - 232) as usize];
            Rgb(v, v, v)
        }
    }
}

/// Weighted Euclidean color distance (ITU-R BT.601 luminance weights).
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

/// Map an RGB triple to the nearest 256-color palette index.
///
/// Ports `rgb_to_256` from `crates/pi/src/modes/interactive/theme.rs`.
/// Returns indices in the range 16–255 (cube or grayscale), never 0–15.
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

/// Downsample an RGB triple to the 256-color palette and return the
/// palette-resolved RGB (what the terminal actually displays).
#[must_use]
pub fn downsample_256(rgb: Rgb) -> Rgb {
    palette_rgb(rgb_to_256(rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_index_16_is_black() {
        assert_eq!(palette_rgb(16), Rgb(0, 0, 0));
    }

    #[test]
    fn cube_index_231_is_white() {
        assert_eq!(palette_rgb(231), Rgb(255, 255, 255));
    }

    #[test]
    fn gray_index_232_is_8() {
        assert_eq!(palette_rgb(232), Rgb(8, 8, 8));
    }

    #[test]
    fn gray_index_255_is_238() {
        assert_eq!(palette_rgb(255), Rgb(238, 238, 238));
    }

    #[test]
    fn pure_white_downsamples_to_cube_white() {
        assert_eq!(rgb_to_256(Rgb(255, 255, 255)), 231);
    }

    #[test]
    fn pure_black_downsamples_to_cube_black() {
        assert_eq!(rgb_to_256(Rgb(0, 0, 0)), 16);
    }

    #[test]
    fn gray_downsamples_to_gray_ramp() {
        let idx = rgb_to_256(Rgb(128, 128, 128));
        assert!((232..=255).contains(&idx), "got {idx}");
    }
}
