// SPDX-License-Identifier: AGPL-3.0-only
//! An optional HPLuv/HSL palette, ported from the `perfect-palette`
//! helper in the user's Emacs configuration.
//!
//! The palette walks the hue circle while keeping saturation and lightness
//! fixed, which produces a set of colors that feel equally bright. MuReader
//! uses it to soften the reader's own theme: the hue family comes from the
//! existing window, text, and accent colors, then the paper, ink, accent,
//! search, and selection tones are regenerated around it.
//!
//! This module never decides whether the feature is enabled. The caller only
//! appends [`css`] to the user's stylesheet when the option is on, so a
//! disabled palette leaves the original CSS and theme exactly untouched.

use std::f64::consts::PI;

use crate::theme::Theme;

/// Keep the generated stylesheet bounded even if the config asks for a huge
/// palette.
const MAX_COLORS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Number of colors to generate (`p` in the Emacs helper).
    pub colors: usize,
    /// Hue step (`q`); the actual step is `q / colors` turns.
    pub step: usize,
    /// Hue used when the existing theme is fully neutral, as a fraction of a
    /// turn. The window, accent, or text hue anchors the palette whenever one
    /// of them has any tint.
    pub hue_start: f32,
    /// Saturation in percent for the generated palette colors.
    pub saturation: f32,
    /// Lightness in percent for the generated palette colors. The reading
    /// surface itself follows the existing theme, so light and dark themes
    /// keep their character.
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

/// Generate an eye-comfort palette from the theme's own background, text, and
/// accent hues. A tinted theme keeps its hue family; `hue_start` is only the
/// fallback for a neutral theme such as plain white and black CSS.
pub fn tune(theme: Theme, options: Options) -> Palette {
    let options = options.normalized();
    let (accent_hue, accent_saturation, _) = source_color(theme.accent, options.hpluv);
    let (window_hue, window_saturation, window_lightness) =
        source_color(theme.window, options.hpluv);
    let (text_hue, text_saturation, text_lightness) = source_color(theme.text, options.hpluv);
    let fallback_hue = options.hue_start * 360.0;
    let base_hue = if accent_saturation >= 5.0 {
        accent_hue
    } else if window_saturation >= 2.0 {
        window_hue
    } else if text_saturation >= 5.0 {
        text_hue
    } else {
        fallback_hue
    };
    let dark = theme.dark;

    let hues = palette_hues(base_hue, &options);
    let colors: Vec<u32> = hues
        .iter()
        .map(|hue| convert(*hue, options.saturation, options.lightness, options.hpluv))
        .collect();

    let paper_hue = if window_saturation >= 2.0 {
        window_hue
    } else {
        base_hue
    };
    let paper_saturation = if window_saturation >= 2.0 {
        (window_saturation * 0.9).clamp(1.5, 100.0)
    } else {
        (options.saturation * 0.20).clamp(1.5, 10.0)
    };
    let paper_lightness = if dark {
        window_lightness.clamp(10.0, 16.0)
    } else {
        window_lightness.clamp(90.0, 96.5)
    };
    let background = convert(paper_hue, paper_saturation, paper_lightness, options.hpluv);

    let ink_hue = if text_saturation >= 5.0 {
        text_hue
    } else {
        base_hue
    };
    let ink_saturation = if text_saturation >= 2.0 {
        (text_saturation * 0.9).clamp(1.5, 100.0)
    } else {
        (options.saturation * 0.20).clamp(2.0, 16.0)
    };
    let ink_lightness = if dark {
        text_lightness.clamp(84.0, 92.0)
    } else {
        text_lightness.clamp(16.0, 26.0)
    };
    let text = convert(ink_hue, ink_saturation, ink_lightness, options.hpluv);

    let accent_hue = if accent_saturation >= 5.0 {
        accent_hue
    } else {
        base_hue
    };
    let accent = convert(
        accent_hue,
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

/// Read a color in the space the palette renders in, so hue, saturation, and
/// lightness keep their meaning across the transformation.
fn source_color(color: u32, hpluv: bool) -> (f32, f32, f32) {
    if hpluv {
        rgb_to_hpluv(color)
    } else {
        rgb_to_hsl(color)
    }
}

fn palette_hues(base_hue: f32, options: &Options) -> Vec<f32> {
    (0..options.colors)
        .map(|index| {
            (base_hue + index as f32 * options.step as f32 / options.colors as f32 * 360.0)
                .rem_euclid(360.0)
        })
        .collect()
}

fn rgb_to_hsl(color: u32) -> (f32, f32, f32) {
    let r = ((color >> 16) & 0xff) as f32 / 255.0;
    let g = ((color >> 8) & 0xff) as f32 / 255.0;
    let b = (color & 0xff) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness * 100.0);
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let hue = if max == r {
        ((g - b) / delta + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };
    (hue * 360.0, saturation * 100.0, lightness * 100.0)
}

/// Convert an sRGB color to HPLuv: hue in degrees, saturation in percent, and
/// lightness in percent. This is the inverse of [`hpluv_to_rgb`] without
/// rounding through 8-bit channels.
fn rgb_to_hpluv(color: u32) -> (f32, f32, f32) {
    let (hue, chroma, lightness) = rgb_to_lch(color);
    if lightness <= 1.0e-9 {
        return (0.0, 0.0, 0.0);
    }
    let max = max_safe_chroma_for_l(lightness);
    let saturation = if max > 1.0e-9 {
        (chroma / max * 100.0) as f32
    } else {
        0.0
    };
    (hue as f32, saturation, lightness as f32)
}

fn rgb_to_lch(color: u32) -> (f64, f64, f64) {
    let channel = |shift: u32| {
        let value = ((color >> shift) & 0xff) as f64 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let r = channel(16);
    let g = channel(8);
    let b = channel(0);
    let x = TO_XYZ[0][0] * r + TO_XYZ[0][1] * g + TO_XYZ[0][2] * b;
    let y = TO_XYZ[1][0] * r + TO_XYZ[1][1] * g + TO_XYZ[1][2] * b;
    let z = TO_XYZ[2][0] * r + TO_XYZ[2][1] * g + TO_XYZ[2][2] * b;
    let lightness = if y > EPSILON {
        116.0 * y.cbrt() - 16.0
    } else {
        KAPPA * y
    };
    let denominator = x + 15.0 * y + 3.0 * z;
    if denominator.abs() <= 1.0e-12 || lightness <= 1.0e-9 {
        return (0.0, 0.0, lightness);
    }
    let var_u = 4.0 * x / denominator;
    let var_v = 9.0 * y / denominator;
    let u = 13.0 * lightness * (var_u - REF_U);
    let v = 13.0 * lightness * (var_v - REF_V);
    let chroma = (u * u + v * v).sqrt();
    let mut hue = v.atan2(u) * 180.0 / PI;
    if hue < 0.0 {
        hue += 360.0;
    }
    (hue, chroma, lightness)
}

/// Render the tuned palette as CSS appended after the user's reflowable CSS.
/// The declarations reuse the same selectors and tokens, so they transform the
/// existing theme instead of imposing an unrelated palette.
pub fn css(palette: &Palette) -> String {
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

/// Linear sRGB to CIE XYZ (D65), the inverse of [`M`].
const TO_XYZ: [[f64; 3]; 3] = [
    [
        0.412_390_799_265_959,
        0.357_584_339_383_878,
        0.180_480_788_401_834,
    ],
    [
        0.212_639_005_871_510,
        0.715_168_678_767_756,
        0.072_192_315_360_734,
    ],
    [
        0.019_330_818_715_592,
        0.119_194_779_794_626,
        0.950_532_152_249_661,
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
    fn neutral_theme_falls_back_to_hue_start() {
        let palette = tune(Theme::html_default(), Options::default());
        assert_eq!(palette.colors.len(), 9);
        assert_eq!(palette.colors[0], hpluv_to_rgb(208.8, 45.0, 65.0));
        assert_eq!(palette.background, hpluv_to_rgb(208.8, 9.0, 96.5));
        assert_eq!(palette.text, hpluv_to_rgb(208.8, 9.0, 16.0));
        assert_eq!(palette.accent, hpluv_to_rgb(208.8, 45.0, 45.0));
    }

    #[test]
    fn tinted_theme_keeps_its_hue_family() {
        let theme = Theme::from_css(Some(
            "body { background: #f2e8d5; color: #3a2e24; }
             :root { --mureader-accent: #b5762f; }",
        ));
        let palette = tune(theme, Options::default());
        let (accent_hue, _, _) = rgb_to_hpluv(theme.accent);
        let (window_hue, window_saturation, window_lightness) = rgb_to_hpluv(theme.window);
        assert_eq!(palette.colors[0], hpluv_to_rgb(accent_hue, 45.0, 65.0));
        assert_eq!(
            palette.background,
            hpluv_to_rgb(
                window_hue,
                (window_saturation * 0.9).clamp(1.5, 100.0),
                window_lightness.clamp(90.0, 96.5)
            )
        );
        assert_ne!(palette.colors[0], hpluv_to_rgb(208.8, 45.0, 65.0));
    }

    #[test]
    fn hsl_mode_uses_hsl_hues() {
        let theme = Theme::from_css(Some(
            "body { background: #f2e8d5; color: #3a2e24; }
             :root { --mureader-accent: #b5762f; }",
        ));
        let options = Options {
            hpluv: false,
            ..Options::default()
        };
        let palette = tune(theme, options);
        let (accent_hue, _, _) = rgb_to_hsl(theme.accent);
        assert_eq!(palette.colors[0], hsl_to_rgb(accent_hue, 45.0, 65.0));
    }

    #[test]
    fn hpluv_roundtrips_through_the_inverse_conversion() {
        for (color, hue, saturation, lightness) in [
            (0x007d_a4ad, 210.407, 45.007, 64.905),
            (0x001f_8390, 208.316, 99.823, 50.141),
            (0x00ba_949b, 359.517, 44.798, 64.952),
        ] {
            let (actual_hue, actual_saturation, actual_lightness) = rgb_to_hpluv(color);
            assert!(close(actual_hue, hue, 0.1), "{actual_hue} != {hue}");
            assert!(
                close(actual_saturation, saturation, 0.1),
                "{actual_saturation} != {saturation}"
            );
            assert!(
                close(actual_lightness, lightness, 0.1),
                "{actual_lightness} != {lightness}"
            );
        }
    }

    #[test]
    fn palette_css_names_every_color_and_token() {
        let palette = tune(Theme::html_default(), Options::default());
        let css = css(&palette);
        assert!(css.contains("--mureader-palette-0: #7da4ad;"));
        assert!(css.contains(&format!(
            "--mureader-palette-8: {};",
            hex(palette.colors[8])
        )));
        assert!(css.contains(&format!("--mureader-accent: {};", hex(palette.accent))));
        assert!(css.contains(&format!("--mureader-search: {};", hex(palette.search))));
        assert!(css.contains(&format!(
            "--mureader-selection: {};",
            hex(palette.selection)
        )));
        assert!(css.contains(&format!("background-color: {};", hex(palette.background))));
        assert!(css.contains(&format!("color: {};", hex(palette.text))));
    }

    #[test]
    fn dark_theme_tunes_the_dark_reading_surface() {
        let theme = Theme::from_css(Some("body { background: #101010; color: #d0d0d0; }"));
        assert!(theme.dark);
        let palette = tune(theme, Options::default());
        assert!(relative(palette.background) < relative(palette.text));
    }

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
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
