//! Color science: sRGB → Lab, WCAG contrast ratio, and CIEDE2000 ΔE.
//!
//! All formulas follow the canonical references:
//! - WCAG 2.2 §1.4.3 (contrast ratio)
//! - Sharma, Wu, Dalal (2005) "The CIEDE2000 Color-Difference Formula"

/// 8-bit sRGB triple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Convert to linear [0,1] floats.
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

    /// WCAG relative luminance (sRGB, D65).
    fn luminance(self) -> f64 {
        let [r, g, b] = self.to_linear();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Convert to CIE Lab (D65 reference white).
    fn to_lab(self) -> Lab {
        let [r, g, b] = self.to_linear();

        // sRGB → XYZ (D65)
        let x = 0.4124564 * r + 0.3575761 * g + 0.1804375 * b;
        let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
        let z = 0.0193339 * r + 0.1191920 * g + 0.9503041 * b;

        // D65 reference white
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
}

/// CIE L*a*b* color.
#[derive(Clone, Copy, Debug)]
pub struct Lab {
    pub l: f64,
    pub a: f64,
    pub b: f64,
}

/// WCAG 2.2 contrast ratio between two sRGB colors.
///
/// Returns a ratio in [1.0, 21.0]. 1.0 = identical luminance, 21.0 = black vs white.
#[must_use]
pub fn wcag_ratio(fg: Rgb, bg: Rgb) -> f64 {
    let l1 = fg.luminance();
    let l2 = bg.luminance();
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// CIEDE2000 color difference between two sRGB colors.
///
/// Implementation follows Sharma et al. (2005), including the hue-angle
/// interpolation bugfix and the `h̄` selection rule.
#[must_use]
pub fn delta_e_2000(c1: Rgb, c2: Rgb) -> f64 {
    de2000_lab(c1.to_lab(), c2.to_lab())
}

/// CIEDE2000 between two Lab colors (Sharma et al. 2005).
fn de2000_lab(lab1: Lab, lab2: Lab) -> f64 {
    let l1 = lab1.l;
    let a1 = lab1.a;
    let b1 = lab1.b;
    let l2 = lab2.l;
    let a2 = lab2.a;
    let b2 = lab2.b;

    // Step 1: calculate C_i, h_i
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

    // Step 2: calculate ΔL', ΔC', ΔH'
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

    // Step 3: calculate CIEDE2000
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

    let k_l = 1.0;
    let k_c = 1.0;
    let k_h = 1.0;

    let term_l = d_lp / (k_l * s_l);
    let term_c = d_cp / (k_c * s_c);
    let term_h = d_hp / (k_h * s_h);

    (term_l * term_l + term_c * term_c + term_h * term_h + r_t * term_c * term_h).sqrt()
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
    fn identical_colors_ratio_is_1() {
        let r = wcag_ratio(Rgb(128, 64, 200), Rgb(128, 64, 200));
        assert!((r - 1.0).abs() < 1e-9);
    }

    #[test]
    fn de2000_identical_is_zero() {
        let d = delta_e_2000(Rgb(100, 150, 200), Rgb(100, 150, 200));
        assert!(d.abs() < 1e-9, "got {d}");
    }

    // Sharma et al. (2005) test data — ΔE2000 reference values.
    // Each tuple: (Lab1, Lab2, expected ΔE2000)
    const SHARMA_DATA: &[(f64, f64, f64, f64, f64, f64, f64)] = &[
        // (L1,a1,b1, L2,a2,b2, expected)
        (50.0, 2.6772, -79.7751, 50.0, 0.0, -82.7485, 2.0425),
        (50.0, 3.1571, -77.2803, 50.0, 0.0, -82.7485, 2.8615),
        (50.0, 2.8361, -74.0200, 50.0, 0.0, -82.7485, 3.4412),
        (50.0, -1.3802, -84.2814, 50.0, 0.0, -82.7485, 1.0000),
        (50.0, -1.1848, -84.8006, 50.0, 0.0, -82.7485, 1.0000),
        (50.0, -0.9009, -85.5211, 50.0, 0.0, -82.7485, 1.0000),
        (50.0, 0.0, 0.0, 50.0, -1.0, 2.0, 2.3669),
        (50.0, -1.0, 2.0, 50.0, 0.0, 0.0, 2.3669),
        (50.0, 2.49, -0.001, 50.0, -2.49, 0.0009, 7.1792),
        (50.0, 2.49, -0.001, 50.0, -2.49, 0.001, 7.1792),
    ];

    #[test]
    fn de2000_sharma_reference_values() {
        for (i, &(l1, a1, b1, l2, a2, b2, expected)) in SHARMA_DATA.iter().enumerate() {
            let lab1 = Lab { l: l1, a: a1, b: b1 };
            let lab2 = Lab { l: l2, a: a2, b: b2 };
            let d = de2000_lab(lab1, lab2);
            assert!(
                (d - expected).abs() < 0.0001,
                "case {i}: got {d:.4}, expected {expected:.4}"
            );
        }
    }

    #[test]
    fn mid_luminance_wcag_known_value() {
        // #777777 on #000000: WCAG ratio with exponent 2.4 is ~4.69.
        // With the wrong exponent 2.0 it would be ~5.89.
        // This guards the sRGB linearization exponent regression.
        let r = wcag_ratio(Rgb(0x77, 0x77, 0x77), Rgb(0, 0, 0));
        assert!((r - 4.69).abs() < 0.15, "got {r:.4}, expected ~4.69");
    }

    #[test]
    fn light_theme_dim_on_white_passes_aa() {
        // #767676 on #ffffff: the canonical WCAG "just passes 4.5" sample.
        // With exponent 2.0 this would report ~3.61 (fail); with 2.4 it passes.
        let r = wcag_ratio(Rgb(0x76, 0x76, 0x76), Rgb(255, 255, 255));
        assert!(r >= 4.5, "got {r:.4}, expected >= 4.5");
    }
}
