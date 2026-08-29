//! Color science: sRGB → Lab, WCAG contrast ratio, and CIEDE2000 ΔE.
//!
//! Identical formulas to tui-p2-contrast (issue #58), validated against
//! Sharma et al. (2005) reference data. Duplicated here so the V5 prototype
//! stays self-contained.

/// 8-bit sRGB triple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn to_linear(self) -> [f64; 3] {
        fn ch(c: u8) -> f64 {
            let v = f64::from(c) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        [ch(self.0), ch(self.1), ch(self.2)]
    }

    fn luminance(self) -> f64 {
        let [r, g, b] = self.to_linear();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn to_lab(self) -> Lab {
        let [r, g, b] = self.to_linear();
        let x = 0.4124564 * r + 0.3575761 * g + 0.1804375 * b;
        let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
        let z = 0.0193339 * r + 0.1191920 * g + 0.9503041 * b;
        let xn = 0.95047;
        let yn = 1.0;
        let zn = 1.08883;
        fn f(t: f64) -> f64 {
            let delta: f64 = 6.0 / 29.0;
            if t > delta.powi(3) {
                t.cbrt()
            } else {
                t / (3.0 * delta * delta) + 4.0 / 29.0
            }
        }
        let fx = f(x / xn);
        let fy = f(y / yn);
        let fz = f(z / zn);
        Lab {
            l: 116.0 * fy - 16.0,
            a: 500.0 * (fx - fy),
            b: 200.0 * (fy - fz),
        }
    }

    /// BT.601 weighted luminance (0–255), used by the polarity classifier.
    fn bt601_luminance(self) -> u32 {
        (u32::from(self.0) * 299 + u32::from(self.1) * 587 + u32::from(self.2) * 114) / 1000
    }
}

/// CIE L*a*b* color.
#[derive(Clone, Copy, Debug)]
pub struct Lab {
    pub l: f64,
    pub a: f64,
    pub b: f64,
}

/// WCAG 2.2 contrast ratio between two sRGB colors.
#[must_use]
pub fn wcag_ratio(fg: Rgb, bg: Rgb) -> f64 {
    let l1 = fg.luminance();
    let l2 = bg.luminance();
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// CIEDE2000 color difference between two sRGB colors.
#[must_use]
pub fn delta_e_2000(c1: Rgb, c2: Rgb) -> f64 {
    de2000_lab(c1.to_lab(), c2.to_lab())
}

fn de2000_lab(lab1: Lab, lab2: Lab) -> f64 {
    let l1 = lab1.l;
    let a1 = lab1.a;
    let b1 = lab1.b;
    let l2 = lab2.l;
    let a2 = lab2.a;
    let b2 = lab2.b;

    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let c_bar = (c1 + c2) / 2.0;
    let g = 0.5 * (1.0 - (c_bar.powi(7) / (c_bar.powi(7) + 25f64.powi(7))).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = (a1p * a1p + b1 * b1).sqrt();
    let c2p = (a2p * a2p + b2 * b2).sqrt();
    let h1p = if a1p == 0.0 && b1 == 0.0 {
        0.0
    } else {
        let h = b1.atan2(a1p).to_degrees();
        if h < 0.0 { h + 360.0 } else { h }
    };
    let h2p = if a2p == 0.0 && b2 == 0.0 {
        0.0
    } else {
        let h = b2.atan2(a2p).to_degrees();
        if h < 0.0 { h + 360.0 } else { h }
    };
    let d_lp = l2 - l1;
    let d_cp = c2p - c1p;
    let d_hp_small = (h2p - h1p).abs();
    let d_hp = if c1p * c2p == 0.0 {
        0.0
    } else if d_hp_small <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let d_hp = 2.0 * (c1p * c2p).sqrt() * (d_hp.to_radians() / 2.0).sin();
    let l_bar = (l1 + l2) / 2.0;
    let c_bar_p = (c1p + c2p) / 2.0;
    let h_bar_p = if c1p * c2p == 0.0 {
        h1p + h2p
    } else {
        let h_diff = (h1p - h2p).abs();
        if h_diff <= 180.0 {
            (h1p + h2p) / 2.0
        } else if h1p + h2p < 360.0 {
            (h1p + h2p + 360.0) / 2.0
        } else {
            (h1p + h2p - 360.0) / 2.0
        }
    };
    let t = 1.0
        - 0.17 * (h_bar_p - 30.0).to_radians().cos()
        + 0.24 * (2.0 * h_bar_p).to_radians().cos()
        + 0.32 * (3.0 * h_bar_p + 6.0).to_radians().cos()
        - 0.20 * (4.0 * h_bar_p - 63.0).to_radians().cos();
    let d_theta = 30.0 * (-((h_bar_p - 275.0) / 25.0).powi(2)).exp();
    let d_c7 = c_bar_p.powi(7);
    let r_c = 2.0 * (d_c7 / (d_c7 + 25f64.powi(7))).sqrt();
    let s_l = 1.0 + (0.015 * (l_bar - 50.0).powi(2)) / (20.0 + (l_bar - 50.0).powi(2)).sqrt();
    let s_c = 1.0 + 0.045 * c_bar_p;
    let s_h = 1.0 + 0.015 * c_bar_p * t;
    let r_t = -((2.0 * d_theta).to_radians()).sin() * r_c;
    let term_l = d_lp / s_l;
    let term_c = d_cp / s_c;
    let term_h = d_hp / s_h;
    (term_l * term_l + term_c * term_c + term_h * term_h + r_t * term_c * term_h).sqrt()
}

/// Parse a `#rrggbb` hex string into an [`Rgb`].
pub fn parse_hex(hex: &str) -> Option<Rgb> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

/// Classify polarity from an OSC 11 background reply payload.
///
/// Returns `Some(true)` for dark, `Some(false)` for light, `None` when
/// unparseable. Mirrors `classify_background` in `probe.rs`.
pub fn classify_background_osc11(payload: &str) -> Option<bool> {
    let rgb = parse_background_rgb(payload)?;
    let lum = (u32::from(rgb.0) * 299 + u32::from(rgb.1) * 587 + u32::from(rgb.2) * 114) / 1000;
    Some(lum < 128)
}

/// Parse OSC 11 reply forms: `rgb:RR/GG/BB`, `#RRGGBB`, `#RRRRGGGGBBBB`.
fn parse_background_rgb(payload: &str) -> Option<Rgb> {
    let p = payload.trim();
    if let Some(rest) = p.strip_prefix("rgb:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let r = u16::from_str_radix(parts[0], 16).ok()?;
            let g = u16::from_str_radix(parts[1], 16).ok()?;
            let b = u16::from_str_radix(parts[2], 16).ok()?;
            // Scale 16-bit components to 8-bit.
            return Some(Rgb(
                (r >> 8) as u8,
                (g >> 8) as u8,
                (b >> 8) as u8,
            ));
        }
    }
    if let Some(hex) = p.strip_prefix('#') {
        if hex.len() == 6 {
            return parse_hex(p);
        }
        if hex.len() == 12 {
            let r = u16::from_str_radix(&hex[0..4], 16).ok()?;
            let g = u16::from_str_radix(&hex[4..8], 16).ok()?;
            let b = u16::from_str_radix(&hex[8..12], 16).ok()?;
            return Some(Rgb(
                (r >> 8) as u8,
                (g >> 8) as u8,
                (b >> 8) as u8,
            ));
        }
    }
    None
}

/// ANSI-256 RGB table for COLORFGBG index lookup.
pub const ANSI16_RGB_TABLE: [[u8; 3]; 16] = [
    [0, 0, 0],
    [128, 0, 0],
    [0, 128, 0],
    [128, 128, 0],
    [0, 0, 128],
    [128, 0, 128],
    [0, 128, 128],
    [192, 192, 192],
    [128, 128, 128],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [0, 0, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

/// Classify polarity from a COLORFGBG env var value.
///
/// Returns `Some(true)` for dark, `Some(false)` for light, `None` when
/// unparseable. Mirrors `detect_terminal_theme`'s COLORFGBG path.
pub fn classify_background_colorfgbg(colorfgbg: &str) -> Option<bool> {
    let index = colorfgbg
        .split(';')
        .rev()
        .find_map(|part| part.trim().parse::<u8>().ok())?;
    let [r, g, b] = if index < 16 {
        ANSI16_RGB_TABLE[index as usize]
    } else {
        ansi256_rgb(index)
    };
    let lum = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
    Some(lum < 128)
}

/// Standard xterm 256-color RGB for a palette index.
pub fn ansi256_rgb(index: u8) -> [u8; 3] {
    if index < 16 {
        return ANSI16_RGB_TABLE[index as usize];
    }
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if index <= 231 {
        let i = index - 16;
        return [
            CUBE[(i / 36) as usize],
            CUBE[((i % 36) / 6) as usize],
            CUBE[(i % 6) as usize],
        ];
    }
    let v = 8 + (index - 232) * 10;
    [v, v, v]
}

/// Parse an OSC 11 or `#rrggbb` payload into an [`Rgb`].
pub fn parse_hex_or_rgb(payload: &str) -> Rgb {
    parse_background_rgb(payload).unwrap_or(Rgb(0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_white_ratio_is_21() {
        let r = wcag_ratio(Rgb(0, 0, 0), Rgb(255, 255, 255));
        assert!((r - 21.0).abs() < 0.01, "got {r}");
    }

    #[test]
    fn de2000_identical_is_zero() {
        let d = delta_e_2000(Rgb(100, 150, 200), Rgb(100, 150, 200));
        assert!(d.abs() < 1e-9);
    }

    #[test]
    fn osc11_rgb_classifies_dark() {
        assert_eq!(classify_background_osc11("rgb:0000/0000/0000"), Some(true));
    }

    #[test]
    fn osc11_rgb_classifies_light() {
        assert_eq!(
            classify_background_osc11("rgb:ffff/ffff/ffff"),
            Some(false)
        );
    }

    #[test]
    fn osc11_hex_classifies_dark() {
        assert_eq!(classify_background_osc11("#000000"), Some(true));
    }

    #[test]
    fn osc11_hex12_classifies_light() {
        assert_eq!(
            classify_background_osc11("#ffffffffffff"),
            Some(false)
        );
    }

    #[test]
    fn colorfgbg_classifies_light_bg() {
        // fg=0 (black), bg=15 (white) → light background
        assert_eq!(classify_background_colorfgbg("0;15"), Some(false));
    }

    #[test]
    fn colorfgbg_classifies_dark_bg() {
        // fg=15 (white), bg=0 (black) → dark background
        assert_eq!(classify_background_colorfgbg("15;0"), Some(true));
    }

    #[test]
    fn colorfgbg_unparseable_returns_none() {
        assert_eq!(classify_background_colorfgbg("garbage"), None);
    }
}
