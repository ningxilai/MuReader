// SPDX-License-Identifier: AGPL-3.0-only
//! An optional HPLuv/HSLuv palette, ported from the `perfect-palette`
//! helper in the user's Emacs configuration.
//!
//! The palette walks the hue circle while keeping saturation and lightness
//! fixed, which produces a set of colors that feel equally bright. MuReader
//! uses it for an eye-comfort reading surface: the first hue tints the paper
//! and ink, while the palette supplies the UI accent, search, and selection
//! colors. The generated CSS is the same reflowable stylesheet users already
//! customize, so explicit user colors still win by coming later.

use std::f64::consts::PI;

/// Keep the generated stylesheet bounded even if the config asks for a huge
/// palette.
const MAX_COLORS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Number of colors to generate (`p` in the Emacs helper).
    pub colors: usize,
    /// Hue step (`q`); the actual step is `q / colors` turns.
    pub step: usize,
    /// Starting hue as a fraction of a turn.
    pub hue_start: f32,
    /// Saturation in percent.
    pub saturation: f32,
    /// Lightness in percent. Values below 50 select a dark reading surface.
    pub lightness: f32,
    /// Use HPLuv (`true`) or ordinary HSL (`false`).
    pub hpluv: bool,
}

impl Default for Options {
    fn default() -> Self {
        // The defaults used by the Emacs perfect-palette-override-enable.
        Self {
            colors: 9,
            step: 4,
            hue_start: 0.58,
            saturation: 45.0,
            lightness: 65.0,
            hpluv: true,
        }
    }
}

impl Options {
    fn normalized(self) -> Self {
        Self {
            colors: self.colors.clamp(1, MAX_COLORS),
            step: self.step,
            hue_start: normalize_hue(self.hue_start),
            saturation: self.saturation.clamp(0.0, 100.0),
            lightness: self.lightness.clamp(0.0, 100.0),
            hpluv: self.hpluv,
        }
    }
}

/// A generated reading surface plus the palette colors it was built from.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub colors: Vec<u32>,
    pub background: u32,
    pub text: u32,
    pub accent: u32,
    pub search: u32,
    pub selection: u32,
}

/// Generate the palette and the matching paper/ink/accent colors.
pub fn build(options: Options) -> Palette {
    let options = options.normalized();
    let hues: Vec<f32> = (0..options.colors)
        .map(|index| {
            normalize_hue(
                options.hue_start + index as f32 * options.step as f32 / options.colors as f32,
            )
        })
        .collect();
    let colors: Vec<u32> = hues
        .iter()
        .map(|hue| {
            convert(
                *hue * 360.0,
                options.saturation,
                options.lightness,
                options.hpluv,
            )
        })
        .collect();

    let base_hue = options.hue_start * 360.0;
    let dark = options.lightness < 50.0;
    let paper_saturation = (options.saturation * 0.20).clamp(1.5, 10.0);
    let ink_saturation = (options.saturation * 0.30).clamp(2.0, 16.0);
    let background = convert(
        base_hue,
        paper_saturation,
        if dark { 12.0 } else { 96.0 },
        options.hpluv,
    );
    let text = convert(
        base_hue,
        ink_saturation,
        if dark { 92.0 } else { 18.0 },
        options.hpluv,
    );
    let accent = convert(
        base_hue,
        options.saturation,
        if dark { 72.0 } else { 45.0 },
        options.hpluv,
    );

    let search = closest_hue(&hues, &colors, 55.0).unwrap_or(accent);
    let selection = closest_hue(&hues, &colors, 250.0).unwrap_or(accent);

    Palette {
        colors,
        background,
        text,
        accent,
        search,
        selection,
    }
}

/// Build the CSS block appended before the user's reflowable CSS. The user's
/// later background, text, or `--mureader-*` declarations therefore override
/// any part of this palette without extra configuration.
pub fn css(options: Options) -> String {
    let palette = build(options);
    let mut css = String::new();
    css.push_str(":root {\n");
    for (index, color) in palette.colors.iter().enumerate() {
        css.push_str(&format!(
            "    --mureader-palette-{index}: {};\n",
            hex(*color)
        ));
    }
    css.push_str(&format!(
        "    --mureader-accent: {};\n    --mureader-search: {};\n    --mureader-selection: {};\n",
        hex(palette.accent),
        hex(palette.search),
        hex(palette.selection)
    ));
    css.push_str("}\n");
    css.push_str(&format!(
        "html, body {{\n    background-color: {};\n    color: {};\n}}\n",
        hex(palette.background),
        hex(palette.text)
    ));
    css
}

fn closest_hue(hues: &[f32], colors: &[u32], target: f32) -> Option<u32> {
    hues.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let distance = |hue: f32| {
                let delta = (hue - target).abs() % 360.0;
                delta.min(360.0 - delta)
            };
            distance(**a)
                .partial_cmp(&distance(**b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .and_then(|(index, _)| colors.get(index).copied())
}

fn normalize_hue(hue: f32) -> f32 {
    let hue = hue.rem_euclid(1.0);
    if hue.is_finite() {
        hue
    } else {
        0.0
    }
}

fn convert(hue: f32, saturation: f32, lightness: f32, hpluv: bool) -> u32 {
    if hpluv {
        hpluv_to_rgb(hue, saturation, lightness)
    } else {
        hsl_to_rgb(hue, saturation, lightness)
    }
}

fn hex(color: u32) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (color >> 16) & 0xff,
        (color >> 8) & 0xff,
        color & 0xff
    )
}

/// HPLuv takes a hue in degrees, saturation in percent, and lightness in
/// percent, and returns an `0x00RRGGBB` color.
pub fn hpluv_to_rgb(hue: f32, saturation: f32, lightness: f32) -> u32 {
    let lightness = lightness.clamp(0.0, 100.0) as f64;
    if lightness >= 100.0 - 1.0e-7 {
        return 0x00ff_ffff;
    }
    if lightness <= 1.0e-7 {
        return 0x0000_0000;
    }
    let hue = hue as f64;
    let chroma = max_safe_chroma_for_l(lightness) / 100.0 * saturation.clamp(0.0, 100.0) as f64;
    lch_to_rgb(lightness, chroma, hue)
}

/// Ordinary HSL conversion, matching Emacs `color-hsl-to-rgb`.
pub fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> u32 {
    let hue = (hue as f64).rem_euclid(360.0) / 360.0;
    let saturation = saturation.clamp(0.0, 100.0) as f64 / 100.0;
    let lightness = lightness.clamp(0.0, 100.0) as f64 / 100.0;
    if saturation <= 1.0e-9 {
        let channel = (lightness * 255.0).round() as u32;
        return (channel << 16) | (channel << 8) | channel;
    }
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    let channel = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let value = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (value.clamp(0.0, 1.0) * 255.0).round() as u32
    };
    (channel(hue + 1.0 / 3.0) << 16) | (channel(hue) << 8) | channel(hue - 1.0 / 3.0)
}

fn lch_to_rgb(lightness: f64, chroma: f64, hue: f64) -> u32 {
    let hue = hue.rem_euclid(360.0) * PI / 180.0;
    let u = hue.cos() * chroma;
    let v = hue.sin() * chroma;
    let (x, y, z) = luv_to_xyz(lightness, u, v);
    let r = from_linear(M[0][0] * x + M[0][1] * y + M[0][2] * z);
    let g = from_linear(M[1][0] * x + M[1][1] * y + M[1][2] * z);
    let b = from_linear(M[2][0] * x + M[2][1] * y + M[2][2] * z);
    let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

fn luv_to_xyz(lightness: f64, u: f64, v: f64) -> (f64, f64, f64) {
    if lightness <= 1.0e-9 {
        return (0.0, 0.0, 0.0);
    }
    let var_u = u / (13.0 * lightness) + REF_U;
    let var_v = v / (13.0 * lightness) + REF_V;
    let y = if lightness > 8.0 {
        ((lightness + 16.0) / 116.0).powi(3)
    } else {
        lightness / KAPPA
    };
    let x = -(9.0 * y * var_u) / ((var_u - 4.0) * var_v - var_u * var_v);
    let z = (9.0 * y - 15.0 * var_v * y - var_v * x) / (3.0 * var_v);
    (x, y, z)
}

fn from_linear(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn max_safe_chroma_for_l(lightness: f64) -> f64 {
    let mut min = f64::INFINITY;
    for (slope, intercept) in get_bounds(lightness) {
        let length = intercept.abs() / (slope * slope + 1.0).sqrt();
        if length >= 0.0 && length < min {
            min = length;
        }
    }
    min
}

fn get_bounds(lightness: f64) -> Vec<(f64, f64)> {
    let sub1 = (lightness + 16.0).powi(3) / 1_568_896.0;
    let sub2 = if sub1 > EPSILON {
        sub1
    } else {
        lightness / KAPPA
    };
    let mut bounds = Vec::with_capacity(6);
    for row in M {
        for t in 0..2 {
            let t = t as f64;
            let top1 = (284_517.0 * row[0] - 94_839.0 * row[2]) * sub2;
            let top2 =
                (838_422.0 * row[2] + 769_860.0 * row[1] + 731_718.0 * row[0]) * lightness * sub2
                    - 769_860.0 * t * lightness;
            let bottom = (632_260.0 * row[2] - 126_452.0 * row[1]) * sub2 + 126_452.0 * t;
            bounds.push((top1 / bottom, top2 / bottom));
        }
    }
    bounds
}

const REF_U: f64 = 0.197_830_006_642_83;
const REF_V: f64 = 0.468_319_994_938_79;
const KAPPA: f64 = 903.296_296_2;
const EPSILON: f64 = 0.008_856_451_6;

const M: [[f64; 3]; 3] = [
    [
        3.240_969_941_904_521,
        -1.537_383_177_570_093,
        -0.498_610_760_293,
    ],
    [
        -0.969_243_636_280_87,
        1.875_967_501_507_72,
        0.041_555_057_407_175,
    ],
    [
        0.055_630_079_696_993,
        -0.203_976_958_888_97,
        1.056_971_514_242_878,
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpluv_matches_the_reference_implementation() {
        assert_eq!(hpluv_to_rgb(0.0, 0.0, 100.0), 0x00ff_ffff);
        assert_eq!(hpluv_to_rgb(0.0, 0.0, 0.0), 0x0000_0000);
        assert_eq!(hpluv_to_rgb(208.8, 45.0, 65.0), 0x007d_a4ad);
        assert_eq!(hpluv_to_rgb(208.8, 100.0, 50.0), 0x001f_8390);
        assert_eq!(hpluv_to_rgb(0.0, 45.0, 65.0), 0x00ba_949b);
    }

    #[test]
    fn hsl_fallback_matches_ordinary_hsl() {
        assert_eq!(hsl_to_rgb(0.0, 100.0, 50.0), 0x00ff_0000);
        assert_eq!(hsl_to_rgb(120.0, 100.0, 50.0), 0x0000_ff00);
        assert_eq!(hsl_to_rgb(240.0, 100.0, 50.0), 0x0000_00ff);
        assert_eq!(hsl_to_rgb(0.0, 0.0, 50.0), 0x0080_8080);
    }

    #[test]
    fn perfect_palette_uses_the_emacs_defaults() {
        let options = Options::default();
        let palette = build(options);
        assert_eq!(palette.colors.len(), 9);
        assert_eq!(palette.colors[0], 0x007d_a4ad);
        assert_eq!(palette.background, hpluv_to_rgb(208.8, 9.0, 96.0));
        assert_eq!(palette.text, hpluv_to_rgb(208.8, 13.5, 18.0));
        assert_eq!(palette.accent, hpluv_to_rgb(208.8, 45.0, 45.0));
    }

    #[test]
    fn palette_css_names_every_color_and_lets_user_css_follow() {
        let css = css(Options::default());
        assert!(css.contains("--mureader-palette-0: #7da4ad;"));
        assert!(css.contains("--mureader-palette-8:"));
        assert!(css.contains("--mureader-accent:"));
        assert!(css.contains("background-color:"));
        assert!(css.contains("color:"));
        assert!(css.contains("html, body"));
    }

    #[test]
    fn dark_lightness_flips_the_reading_surface() {
        let options = Options {
            lightness: 30.0,
            ..Options::default()
        };
        let palette = build(options);
        assert!(relative(palette.background) < relative(palette.text));
    }

    fn relative(color: u32) -> f32 {
        let channel = |shift: u32| {
            let value = ((color >> shift) & 0xff) as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }
}
