// SPDX-License-Identifier: AGPL-3.0-only
//! The one source of colors for every non-reading UI surface.
//!
//! The palette is parsed once from the reflowable CSS. Explicit
//! `--mureader-*` custom properties win; otherwise the effective
//! `background`/`background-color` and `color` of `body`, `html`, or `:root`
//! become the base. Panel shades, borders, muted labels, chips, selection
//! washes, and text colors are then derived from that base so the outline,
//! command panel, prompts, and loading card always agree with each other.
//!
//! When the CSS names no colors at all, the palette follows the document
//! instead: it starts from the HTML default canvas (a white page with dark
//! text) and is re-derived from the rendered page background as pages arrive.

use std::collections::BTreeMap;

use crate::config::{strip_css_comments, strip_font_face_blocks};

/// The built-in dark window color, also used when a CSS theme only overrides
/// individual tokens.
pub const DEFAULT_WINDOW: u32 = 0x0018_1818;
/// The built-in warm accent.
pub const DEFAULT_ACCENT: u32 = 0x00d7_a45e;
/// The relative-luminance crossover where dark text becomes more readable
/// than light text (the WCAG black/white contrast crossover).
const DARK_TEXT_THRESHOLD: f32 = 0.179;

/// Colors baked into the worker-side outline bitmap. The veil and rail depend
/// on the current page background, so the UI computes those and passes them
/// separately with each request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutlineStyle {
    pub panel: u32,
    pub text: u32,
    pub muted: u32,
    pub selected_text: u32,
}

impl Default for OutlineStyle {
    fn default() -> Self {
        Self {
            panel: 0x0020_2020,
            text: 0x00e6_e6e6,
            muted: 0x00b8_b8b8,
            selected_text: 0x00ff_ffff,
        }
    }
}

/// Semantic colors shared by all non-reading UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub window: u32,
    /// Base surface of the command panel.
    pub panel: u32,
    pub panel_raised: u32,
    pub panel_sunken: u32,
    pub panel_selected: u32,
    pub panel_border: u32,
    pub separator: u32,
    pub scrollbar_track: u32,
    pub scrollbar_thumb: u32,
    pub chip: u32,
    pub chip_selected: u32,
    pub chip_border: u32,
    pub chip_border_selected: u32,
    pub prompt: u32,
    pub prompt_border: u32,
    pub loading: u32,
    pub loading_border: u32,
    pub outline_border: u32,
    pub accent: u32,
    pub text_title: u32,
    pub text: u32,
    pub text_secondary: u32,
    pub text_muted: u32,
    pub text_faint: u32,
    pub text_selected: u32,
    pub key_text: u32,
    pub key_text_selected: u32,
    pub footer_text: u32,
    pub footer_echo: u32,
    pub search_highlight: u32,
    pub search_alpha: f32,
    pub selection_highlight: u32,
    pub selection_alpha: f32,
    pub outline: OutlineStyle,
}

impl Default for Theme {
    fn default() -> Self {
        Self::html_default()
    }
}

impl Theme {
    /// Parse a theme from the combined reflowable CSS. Without any color
    /// declarations this starts from the HTML default canvas; the app then
    /// follows the rendered page background when it becomes available.
    pub fn from_css(css: Option<&str>) -> Self {
        match css {
            Some(css) if !css.trim().is_empty() => {
                let overrides = parse_overrides(css);
                if overrides.any() {
                    Self::from_overrides(overrides)
                } else {
                    Self::html_default()
                }
            }
            _ => Self::html_default(),
        }
    }

    /// Whether the CSS chooses any UI color. When it does not, the app keeps
    /// the theme in sync with the rendered document background.
    pub fn has_css_colors(css: Option<&str>) -> bool {
        match css {
            Some(css) if !css.trim().is_empty() => parse_overrides(css).any(),
            _ => false,
        }
    }

    /// The HTML default canvas: a white page with dark text.
    pub fn html_default() -> Self {
        Self::from_document(0x00ff_ffff)
    }

    /// Derive the whole palette from a rendered document background. Used when
    /// the CSS names no colors, so the UI matches the page instead of forcing
    /// the built-in dark shell.
    pub fn from_document(background: u32) -> Self {
        Self::from_overrides(Overrides {
            window: Some(background),
            ..Overrides::default()
        })
    }

    /// The built-in dark palette, kept for partial token overrides such as a
    /// lone `--mureader-accent`.
    pub fn dark() -> Self {
        let accent = DEFAULT_ACCENT;
        let panel = 0x001b_1c1e;
        let separator = 0x0028_2a2d;
        Self {
            dark: true,
            window: DEFAULT_WINDOW,
            panel,
            panel_raised: 0x0020_2124,
            panel_sunken: 0x0019_1a1c,
            panel_selected: 0x0026_2b31,
            panel_border: 0x003d_3f45,
            separator,
            scrollbar_track: blend(panel, separator, 0.8),
            scrollbar_thumb: blend(panel, accent, 0.55),
            chip: 0x002a_2b2f,
            chip_selected: blend(0x002a_2b2f, accent, 0.22),
            chip_border: 0x0037_383e,
            chip_border_selected: blend(0x0037_383e, accent, 0.65),
            prompt: 0x0020_2020,
            prompt_border: 0x0048_4848,
            loading: 0x002a_2a2a,
            loading_border: 0x0050_5050,
            outline_border: 0x003c_3c3c,
            accent,
            text_title: 0x00ec_ecf0,
            text: 0x00e6_e6e6,
            text_secondary: 0x00d6_d6db,
            text_muted: 0x00b8_b8b8,
            text_faint: 0x008e_8f94,
            text_selected: 0x00ff_ffff,
            key_text: 0x00b0_b1b8,
            key_text_selected: 0x00f4_c992,
            footer_text: 0x00a0_a1a6,
            footer_echo: 0x00eb_d7af,
            search_highlight: 0x00ff_e61e,
            search_alpha: 0.45,
            selection_highlight: 0x0050_78d2,
            selection_alpha: 0.38,
            outline: OutlineStyle::default(),
        }
    }

    fn from_overrides(overrides: Overrides) -> Self {
        // A CSS file that only overrides an accent, a border, or a highlight
        // keeps the exact built-in dark palette as its base.
        let has_base =
            overrides.window.is_some() || overrides.text.is_some() || overrides.surface.is_some();
        if !has_base {
            let mut theme = Self::dark();
            if let Some(accent) = overrides.accent {
                theme.apply_accent(accent);
            }
            if let Some(muted) = overrides.muted {
                theme.text_muted = muted;
                theme.outline.muted = muted;
            }
            if let Some(border) = overrides.border {
                theme.apply_border(border);
            }
            if let Some(search) = overrides.search {
                theme.search_highlight = search;
            }
            if let Some(selection) = overrides.selection {
                theme.selection_highlight = selection;
            }
            if let Some(selected_text) = overrides.selected_text {
                theme.text_selected = selected_text;
                theme.outline.selected_text = selected_text;
            }
            return theme;
        }

        let window = overrides.window.unwrap_or(DEFAULT_WINDOW);
        let dark = relative_luminance(window) < DARK_TEXT_THRESHOLD;
        let text = overrides
            .text
            .unwrap_or(if dark { 0x00e6_e6e6 } else { 0x0011_1111 });
        let surface = overrides
            .surface
            .unwrap_or_else(|| shade(window, if dark { 0.05 } else { -0.04 }));
        // Without an explicit accent, derive a neutral tone from the base
        // palette. A custom or document theme must never inherit the built-in
        // warm amber, or the loading rail and which-key chips stand out as a
        // color the rest of the theme does not contain.
        let accent = overrides
            .accent
            .unwrap_or_else(|| blend(text, surface, 0.20));
        let raised = shade(surface, if dark { 0.025 } else { 0.03 });
        let sunken = shade(surface, -0.02);
        let selected = blend(surface, accent, if dark { 0.12 } else { 0.10 });
        let chip = shade(surface, if dark { 0.06 } else { -0.04 });
        let chip_border = blend(chip, text, 0.18);
        let border = overrides.border;
        let outline_border = border.unwrap_or_else(|| blend(surface, text, 0.30));
        let panel_border = border.unwrap_or_else(|| blend(surface, text, 0.30));
        let prompt_border = border.unwrap_or_else(|| blend(surface, text, 0.35));
        let loading = shade(surface, if dark { 0.04 } else { -0.04 });
        let loading_border = border.unwrap_or_else(|| blend(loading, text, 0.35));
        let separator = blend(surface, text, 0.13);
        let muted = overrides
            .muted
            .unwrap_or_else(|| blend(text, surface, 0.26));
        let selected_text = overrides.selected_text.unwrap_or_else(|| {
            if dark {
                0x00ff_ffff
            } else {
                shade(text, -0.35)
            }
        });
        let accent_text = blend(accent, if dark { 0x00ff_ffff } else { 0x0000_0000 }, 0.18);
        Self {
            dark,
            window,
            panel: surface,
            panel_raised: raised,
            panel_sunken: sunken,
            panel_selected: selected,
            panel_border,
            separator,
            scrollbar_track: blend(surface, separator, 0.8),
            scrollbar_thumb: blend(surface, accent, 0.55),
            chip,
            chip_selected: blend(chip, accent, 0.22),
            chip_border,
            chip_border_selected: blend(chip_border, accent, 0.65),
            prompt: surface,
            prompt_border,
            loading,
            loading_border,
            outline_border,
            accent,
            text_title: shade(text, if dark { 0.08 } else { -0.05 }),
            text,
            text_secondary: blend(text, surface, 0.09),
            text_muted: muted,
            text_faint: blend(text, surface, 0.45),
            text_selected: selected_text,
            key_text: muted,
            key_text_selected: accent_text,
            footer_text: blend(text, surface, 0.36),
            footer_echo: accent_text,
            search_highlight: overrides.search.unwrap_or(0x00ff_e61e),
            search_alpha: 0.45,
            selection_highlight: overrides.selection.unwrap_or(0x0050_78d2),
            selection_alpha: 0.38,
            outline: OutlineStyle {
                panel: surface,
                text,
                muted,
                selected_text,
            },
        }
    }

    fn apply_accent(&mut self, accent: u32) {
        self.accent = accent;
        self.chip_selected = blend(self.chip, accent, 0.22);
        self.chip_border_selected = blend(self.chip_border, accent, 0.65);
        self.scrollbar_thumb = blend(self.panel, accent, 0.55);
        let accent_text = blend(
            accent,
            if self.dark { 0x00ff_ffff } else { 0x0000_0000 },
            0.18,
        );
        self.key_text_selected = accent_text;
        self.footer_echo = accent_text;
    }

    fn apply_border(&mut self, border: u32) {
        self.panel_border = border;
        self.prompt_border = border;
        self.loading_border = border;
        self.outline_border = border;
    }
}

/// Blend two `0x00RRGGBB` colors. `t` is clamped to `[0, 1]`.
pub fn blend(base: u32, overlay: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |shift: u32| {
        let from = ((base >> shift) & 0xff) as f32;
        let to = ((overlay >> shift) & 0xff) as f32;
        (from + (to - from) * t).round() as u32
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// Split a packed color into the tuple expected by the text renderer.
pub fn rgb8(color: u32) -> (u8, u8, u8) {
    (
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    )
}

/// Relative luminance in `[0, 1]`, used to choose text contrast.
pub fn relative_luminance(color: u32) -> f32 {
    fn channel(value: u32) -> f32 {
        let value = value as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel((color >> 16) & 0xff)
        + 0.7152 * channel((color >> 8) & 0xff)
        + 0.0722 * channel(color & 0xff)
}

/// WCAG-style contrast ratio between two opaque colors.
#[cfg(test)]
pub fn contrast_ratio(a: u32, b: u32) -> f32 {
    let a = relative_luminance(a);
    let b = relative_luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Move a color towards white (`amount > 0`) or black (`amount < 0`).
fn shade(color: u32, amount: f32) -> u32 {
    if amount >= 0.0 {
        blend(color, 0x00ff_ffff, amount)
    } else {
        blend(color, 0x0000_0000, -amount)
    }
}

#[derive(Debug, Default)]
struct Overrides {
    window: Option<u32>,
    surface: Option<u32>,
    text: Option<u32>,
    muted: Option<u32>,
    accent: Option<u32>,
    border: Option<u32>,
    search: Option<u32>,
    selection: Option<u32>,
    selected_text: Option<u32>,
}

impl Overrides {
    fn any(&self) -> bool {
        self.window.is_some()
            || self.surface.is_some()
            || self.text.is_some()
            || self.muted.is_some()
            || self.accent.is_some()
            || self.border.is_some()
            || self.search.is_some()
            || self.selection.is_some()
            || self.selected_text.is_some()
    }
}

fn parse_overrides(css: &str) -> Overrides {
    let declarations = declarations(css);
    let mut variables = BTreeMap::new();
    for declaration in &declarations {
        if declaration.property.starts_with("--") {
            variables.insert(declaration.property.clone(), declaration.value.clone());
        }
    }
    let token = |name: &str| {
        variables
            .get(name)
            .and_then(|value| parse_color_value(value, &variables))
    };
    Overrides {
        window: token("--mureader-background").or_else(|| {
            find_base_color(
                &declarations,
                &variables,
                &["background-color", "background"],
            )
        }),
        surface: token("--mureader-surface"),
        text: token("--mureader-text")
            .or_else(|| find_base_color(&declarations, &variables, &["color"])),
        muted: token("--mureader-muted"),
        accent: token("--mureader-accent"),
        border: token("--mureader-border"),
        search: token("--mureader-search"),
        selection: token("--mureader-selection"),
        selected_text: token("--mureader-selected-text"),
    }
}

#[derive(Debug, Clone)]
struct Declaration {
    selector: String,
    property: String,
    value: String,
}

fn declarations(css: &str) -> Vec<Declaration> {
    let cleaned = strip_font_face_blocks(&strip_css_comments(css));
    let mut result = Vec::new();
    parse_declarations(&cleaned, "", &mut result);
    result
}

fn parse_declarations(css: &str, selector: &str, out: &mut Vec<Declaration>) {
    let bytes = css.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut parens = 0i32;
    let mut quote = 0u8;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'(' => parens += 1,
            b')' => parens = (parens - 1).max(0),
            b'{' if parens == 0 => {
                let prelude = css[start..index].trim();
                let Some(end) = matching_brace(css, index) else {
                    return;
                };
                let nested = if prelude.starts_with('@') {
                    selector.to_string()
                } else {
                    prelude.to_string()
                };
                parse_declarations(&css[index + 1..end], &nested, out);
                index = end + 1;
                start = index;
                continue;
            }
            b';' if parens == 0 => {
                push_declaration(&css[start..index], selector, out);
                index += 1;
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    push_declaration(&css[start..], selector, out);
}

fn matching_brace(css: &str, open: usize) -> Option<usize> {
    let bytes = css.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote = 0u8;
    let mut index = open;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn push_declaration(raw: &str, selector: &str, out: &mut Vec<Declaration>) {
    let text = raw.trim();
    if text.is_empty() || text.starts_with('@') {
        return;
    }
    let Some(colon) = top_level_colon(text) else {
        return;
    };
    let property = text[..colon].trim();
    let value = strip_important(text[colon + 1..].trim());
    if property.is_empty() || value.is_empty() {
        return;
    }
    let property = if property.starts_with("--") {
        property.to_string()
    } else {
        property.to_ascii_lowercase()
    };
    out.push(Declaration {
        selector: selector.trim().to_string(),
        property,
        value: value.to_string(),
    });
}

fn top_level_colon(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut parens = 0i32;
    let mut quote = 0u8;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'(' => parens += 1,
            b')' => parens = (parens - 1).max(0),
            b':' if parens == 0 => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn strip_important(value: &str) -> &str {
    let lower = value.to_ascii_lowercase();
    match lower.find("!important") {
        Some(index) => value[..index].trim(),
        None => value.trim(),
    }
}

fn find_base_color(
    declarations: &[Declaration],
    variables: &BTreeMap<String, String>,
    properties: &[&str],
) -> Option<u32> {
    let mut best: Option<(u8, u32)> = None;
    for declaration in declarations {
        if !properties.contains(&declaration.property.as_str()) {
            continue;
        }
        let Some(color) = parse_color_value(&declaration.value, variables) else {
            continue;
        };
        let rank = selector_rank(&declaration.selector);
        // Only the document canvas supplies the base. Colors on links,
        // headings, or selection pseudo-elements must not repaint the UI.
        if rank == 0 {
            continue;
        }
        if best.is_none_or(|(best_rank, _)| rank >= best_rank) {
            best = Some((rank, color));
        }
    }
    best.map(|(_, color)| color)
}

fn selector_rank(selector: &str) -> u8 {
    let mut html = false;
    for part in selector.split(',') {
        let part = part.trim().to_ascii_lowercase();
        if is_element_selector(&part, "body") {
            return 3;
        }
        if is_element_selector(&part, "html") {
            html = true;
        }
        if part == ":root" {
            html = true;
        }
    }
    if html {
        2
    } else {
        0
    }
}

fn is_element_selector(part: &str, name: &str) -> bool {
    if part == name {
        return true;
    }
    let Some(rest) = part.strip_prefix(name) else {
        return false;
    };
    if rest.starts_with("::") {
        return false;
    }
    matches!(rest.chars().next(), Some('.' | '#' | '[' | ':'))
}

fn parse_color_value(value: &str, variables: &BTreeMap<String, String>) -> Option<u32> {
    let resolved = resolve_variables(value, variables);
    let resolved = strip_important(&resolved);
    if let Some(color) = parse_color(resolved) {
        return Some(color);
    }
    for token in top_level_tokens(resolved) {
        if let Some(color) = parse_color(token) {
            return Some(color);
        }
    }
    None
}

fn resolve_variables(value: &str, variables: &BTreeMap<String, String>) -> String {
    let mut current = value.to_string();
    for _ in 0..8 {
        let lower = current.to_ascii_lowercase();
        let Some(start) = lower.find("var(") else {
            break;
        };
        let open = start + 3;
        let Some(end) = matching_paren(&current, open) else {
            break;
        };
        let inner = &current[open + 1..end];
        let (name, fallback) = split_top_level_comma(inner);
        let replacement = variables
            .get(name.trim())
            .cloned()
            .or_else(|| fallback.map(str::to_string))
            .unwrap_or_default();
        current.replace_range(start..=end, &replacement);
    }
    current
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote = 0u8;
    let mut index = open;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn split_top_level_comma(value: &str) -> (&str, Option<&str>) {
    let bytes = value.as_bytes();
    let mut parens = 0i32;
    let mut quote = 0u8;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'(' => parens += 1,
            b')' => parens = (parens - 1).max(0),
            b',' if parens == 0 => return (&value[..index], Some(&value[index + 1..])),
            _ => {}
        }
        index += 1;
    }
    (value, None)
}

fn top_level_tokens(value: &str) -> Vec<&str> {
    let bytes = value.as_bytes();
    let mut tokens = Vec::new();
    let mut parens = 0i32;
    let mut quote = 0u8;
    let mut start = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => {
                quote = byte;
                start.get_or_insert(index);
            }
            b'(' => {
                parens += 1;
                start.get_or_insert(index);
            }
            b')' => {
                parens = (parens - 1).max(0);
            }
            _ if byte.is_ascii_whitespace() && parens == 0 => {
                if let Some(token_start) = start.take() {
                    tokens.push(&value[token_start..index]);
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
        index += 1;
    }
    if let Some(token_start) = start {
        tokens.push(&value[token_start..]);
    }
    tokens
}

fn parse_color(input: &str) -> Option<u32> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = value.to_ascii_lowercase();
    if let Some(color) = named_color(&lower) {
        return Some(color);
    }
    let open = lower.find('(')?;
    let name = lower[..open].trim();
    let close = lower.rfind(')')?;
    if close <= open {
        return None;
    }
    let inner = &value[open + 1..close];
    match name {
        "rgb" | "rgba" => parse_rgb(inner),
        "hsl" | "hsla" => parse_hsl(inner),
        _ => None,
    }
}

fn parse_hex(hex: &str) -> Option<u32> {
    if !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |value: &str| u32::from_str_radix(value, 16).ok();
    let (r, g, b, alpha) = match hex.len() {
        3 => (
            digit(&hex[0..1])? * 17,
            digit(&hex[1..2])? * 17,
            digit(&hex[2..3])? * 17,
            255,
        ),
        4 => (
            digit(&hex[0..1])? * 17,
            digit(&hex[1..2])? * 17,
            digit(&hex[2..3])? * 17,
            digit(&hex[3..4])? * 17,
        ),
        6 => (
            digit(&hex[0..2])?,
            digit(&hex[2..4])?,
            digit(&hex[4..6])?,
            255,
        ),
        8 => (
            digit(&hex[0..2])?,
            digit(&hex[2..4])?,
            digit(&hex[4..6])?,
            digit(&hex[6..8])?,
        ),
        _ => return None,
    };
    if alpha == 0 {
        return None;
    }
    Some((r << 16) | (g << 8) | b)
}

fn parse_rgb(inner: &str) -> Option<u32> {
    let parts: Vec<&str> = inner
        .split(|ch: char| ch == ',' || ch == '/' || ch.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let r = parse_channel(parts[0])?;
    let g = parse_channel(parts[1])?;
    let b = parse_channel(parts[2])?;
    if let Some(alpha) = parts.get(3).and_then(|part| parse_alpha(part)) {
        if alpha <= 0.0 {
            return None;
        }
    }
    Some((r << 16) | (g << 8) | b)
}

fn parse_channel(value: &str) -> Option<u32> {
    let value = value.trim();
    let number = if let Some(percent) = value.strip_suffix('%') {
        percent.parse::<f32>().ok()? * 255.0 / 100.0
    } else {
        value.parse::<f32>().ok()?
    };
    Some(number.clamp(0.0, 255.0).round() as u32)
}

fn parse_alpha(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        Some((percent.parse::<f32>().ok()? / 100.0).clamp(0.0, 1.0))
    } else {
        Some(value.parse::<f32>().ok()?.clamp(0.0, 1.0))
    }
}

fn parse_hsl(inner: &str) -> Option<u32> {
    let parts: Vec<&str> = inner
        .split(|ch: char| ch == ',' || ch == '/' || ch.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let hue = parts[0]
        .trim_end_matches("deg")
        .parse::<f32>()
        .ok()?
        .rem_euclid(360.0);
    let saturation = percent_fraction(parts[1])?;
    let lightness = percent_fraction(parts[2])?;
    if let Some(alpha) = parts.get(3).and_then(|part| parse_alpha(part)) {
        if alpha <= 0.0 {
            return None;
        }
    }
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let segment = hue / 60.0;
    let x = chroma * (1.0 - (segment.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match segment as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    let channel = |value: f32| ((value + m).clamp(0.0, 1.0) * 255.0).round() as u32;
    Some((channel(r) << 16) | (channel(g) << 8) | channel(b))
}

fn percent_fraction(value: &str) -> Option<f32> {
    let value = value.trim();
    let number = value.strip_suffix('%')?;
    Some((number.parse::<f32>().ok()? / 100.0).clamp(0.0, 1.0))
}

fn named_color(name: &str) -> Option<u32> {
    match name {
        "transparent" | "none" | "currentcolor" | "inherit" | "initial" | "unset" => None,
        "black" => Some(0x0000_0000),
        "white" => Some(0x00ff_ffff),
        "red" => Some(0x00ff_0000),
        "green" => Some(0x0000_8000),
        "blue" => Some(0x0000_00ff),
        "yellow" => Some(0x00ff_ff00),
        "gray" | "grey" => Some(0x0080_8080),
        "silver" => Some(0x00c0_c0c0),
        "maroon" => Some(0x0080_0000),
        "olive" => Some(0x0080_8000),
        "lime" => Some(0x0000_ff00),
        "aqua" | "cyan" => Some(0x0000_ffff),
        "teal" => Some(0x0000_8080),
        "navy" => Some(0x0000_0080),
        "fuchsia" | "magenta" => Some(0x00ff_00ff),
        "purple" => Some(0x0080_0080),
        "orange" => Some(0x00ff_a500),
        "brown" => Some(0x00a5_2a2a),
        "pink" => Some(0x00ff_c0cb),
        "gold" => Some(0x00ff_d700),
        "beige" => Some(0x00f5_f5dc),
        "ivory" => Some(0x00ff_fff0),
        "khaki" => Some(0x00f0_e68c),
        "lavender" => Some(0x00e6_e6fa),
        "salmon" => Some(0x00fa_8072),
        "tomato" => Some(0x00ff_6347),
        "turquoise" => Some(0x0040_e0d0),
        "violet" => Some(0x00ee_82ee),
        "wheat" => Some(0x00f5_deb3),
        "tan" => Some(0x00d2_b48c),
        "plum" => Some(0x00dd_a0dd),
        "orchid" => Some(0x00da_70d6),
        "indigo" => Some(0x004b_0082),
        "crimson" => Some(0x00dc_143c),
        "coral" => Some(0x00ff_7f50),
        "chocolate" => Some(0x00d2_691e),
        "forestgreen" => Some(0x0022_8b22),
        "seagreen" => Some(0x002e_8b57),
        "skyblue" => Some(0x0087_ceeb),
        "steelblue" => Some(0x0046_82b4),
        "slategray" | "slategrey" => Some(0x0070_8090),
        "midnightblue" => Some(0x0019_1970),
        "whitesmoke" => Some(0x00f5_f5f5),
        "gainsboro" => Some(0x00dc_dcdc),
        "lightgray" | "lightgrey" => Some(0x00d3_d3d3),
        "darkgray" | "darkgrey" => Some(0x00a9_a9a9),
        "dimgray" | "dimgrey" => Some(0x0069_6969),
        "rebeccapurple" => Some(0x0066_3399),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_only_css_has_no_colors_and_follows_the_html_default() {
        let css = Some(r#"body { font-family: "FangSongCode"; }"#);
        assert!(!Theme::has_css_colors(css));
        let theme = Theme::from_css(css);
        assert_eq!(theme, Theme::html_default());
        assert_eq!(theme, Theme::from_document(0x00ff_ffff));
        assert!(!theme.dark);
        assert_eq!(theme.window, 0x00ff_ffff);
        assert_eq!(theme.panel, 0x00f5_f5f5);
        assert_eq!(theme.text, 0x0011_1111);
        assert_eq!(theme.outline.panel, theme.panel);
    }

    #[test]
    fn unrelated_element_colors_do_not_repaint_the_ui() {
        let css = r#"
            body { font-family: "FangSongCode"; }
            a { color: #00ff00; }
            h1 { background-color: #ff0000; }
            ::selection { background: #0000ff; }
        "#;
        assert!(!Theme::has_css_colors(Some(css)));
        assert_eq!(Theme::from_css(Some(css)), Theme::html_default());
    }

    #[test]
    fn document_theme_tracks_light_and_dark_pages() {
        let light = Theme::from_document(0x00ff_ffff);
        assert!(!light.dark);
        assert_eq!(light.window, 0x00ff_ffff);
        assert_eq!(light.outline.panel, light.panel);

        let dark = Theme::from_document(0x0010_1010);
        assert!(dark.dark);
        assert_eq!(dark.window, 0x0010_1010);
        assert_eq!(dark.outline.selected_text, 0x00ff_ffff);
    }

    #[test]
    fn base_theme_without_accent_avoids_the_builtin_amber() {
        let themes = [
            Theme::from_document(0x00ff_ffff),
            Theme::from_css(Some("body { background: #f5e6c8; color: #33291f; }")),
        ];
        for theme in themes {
            assert_ne!(theme.accent, DEFAULT_ACCENT);
            assert_eq!(theme.accent, blend(theme.text, theme.panel, 0.20));
        }
        let document = Theme::from_document(0x00ff_ffff);
        assert_eq!(
            document.accent >> 16 & 0xff,
            document.accent & 0xff,
            "the document-derived accent must stay neutral"
        );
    }

    #[test]
    fn explicit_css_colors_pin_the_theme() {
        let css = Some("body { background: #f5e6c8; color: #33291f; }");
        assert!(Theme::has_css_colors(css));
        assert_eq!(Theme::from_css(css).window, 0x00f5_e6c8);
    }

    #[test]
    fn parses_hex_rgb_and_named_colors() {
        assert_eq!(parse_color("#123456"), Some(0x0012_3456));
        assert_eq!(parse_color("#abc"), Some(0x00aa_bbcc));
        assert_eq!(parse_color("rgb(18, 52, 86)"), Some(0x0012_3456));
        assert_eq!(parse_color("rgba(18 52 86 / 50%)"), Some(0x0012_3456));
        assert_eq!(parse_color("hsl(0, 100%, 50%)"), Some(0x00ff_0000));
        assert_eq!(parse_color("white"), Some(0x00ff_ffff));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("rgba(0, 0, 0, 0)"), None);
    }

    #[test]
    fn body_background_beats_html_and_shorthand_finds_the_color() {
        let css = r#"
            html { background: #000000; }
            body { background: #f5e6c8 url("paper.png") no-repeat; }
        "#;
        let theme = Theme::from_css(Some(css));
        assert_eq!(theme.window, 0x00f5_e6c8);
        assert!(!theme.dark);
    }

    #[test]
    fn custom_properties_override_the_derived_palette() {
        let css = r#"
            :root {
                --mureader-background: #202428;
                --mureader-surface: #2a2e34;
                --mureader-text: #f0f0f0;
                --mureader-accent: #e0a050;
            }
            body { background: #000000; color: #ffffff; }
        "#;
        let theme = Theme::from_css(Some(css));
        assert_eq!(theme.window, 0x0020_2428);
        assert_eq!(theme.panel, 0x002a_2e34);
        assert_eq!(theme.outline.panel, 0x002a_2e34);
        assert_eq!(theme.text, 0x00f0_f0f0);
        assert_eq!(theme.accent, 0x00e0_a050);
        assert_eq!(theme.chip_selected, blend(theme.chip, theme.accent, 0.22));
    }

    #[test]
    fn variable_fallback_resolves() {
        let css = r#"
            :root {
                --paper: #faf3e0;
                --mureader-surface: var(--missing, #334455);
            }
            body { background: var(--paper); color: rgb(30 30 30); }
        "#;
        let theme = Theme::from_css(Some(css));
        assert_eq!(theme.window, 0x00fa_f3e0);
        assert_eq!(theme.panel, 0x0033_4455);
        assert_eq!(theme.text, 0x001e_1e1e);
    }

    #[test]
    fn light_theme_keeps_selected_text_more_contrasty_than_body_text() {
        let css = "body { background: #f5e6c8; color: #33291f; }";
        let theme = Theme::from_css(Some(css));
        assert!(!theme.dark);
        assert!(
            contrast_ratio(theme.outline.text, theme.outline.panel) >= 4.5,
            "body text should stay readable on the derived panel"
        );
        assert!(
            contrast_ratio(theme.outline.selected_text, theme.outline.panel)
                > contrast_ratio(theme.outline.text, theme.outline.panel),
            "selection must increase contrast without changing hue"
        );
        assert!(
            relative_luminance(theme.outline.selected_text)
                < relative_luminance(theme.outline.text),
            "selected text should darken on a light panel"
        );
    }

    #[test]
    fn medium_background_picks_the_readable_text_side() {
        let theme = Theme::from_css(Some("body { background: #808080; }"));
        assert!(!theme.dark);
        assert!(
            contrast_ratio(theme.text, theme.outline.panel) >= 4.4,
            "mid-tone backgrounds should still derive readable text"
        );
    }

    #[test]
    fn dark_theme_selection_is_white() {
        let css = "body { background: #101010; color: #d0d0d0; }";
        let theme = Theme::from_css(Some(css));
        assert!(theme.dark);
        assert_eq!(theme.outline.selected_text, 0x00ff_ffff);
    }

    #[test]
    fn perfect_palette_css_drives_the_ui_theme() {
        let options = crate::palette::Options::default();
        let palette = crate::palette::tune(Theme::html_default(), options);
        let theme = Theme::from_css(Some(&crate::palette::css(&palette)));
        assert_eq!(theme.window, palette.background);
        assert_eq!(theme.text, palette.text);
        assert_eq!(theme.accent, palette.accent);
        assert_eq!(theme.search_highlight, palette.search);
        assert_eq!(theme.selection_highlight, palette.selection);
        assert_eq!(
            theme.dark,
            relative_luminance(palette.background) < DARK_TEXT_THRESHOLD
        );
        assert_eq!(theme.outline.panel, theme.panel);
    }

    #[test]
    fn accent_only_override_keeps_the_dark_palette() {
        let css = Some(":root { --mureader-accent: #66aaff; }");
        assert!(Theme::has_css_colors(css));
        let theme = Theme::from_css(css);
        assert_eq!(theme.window, 0x0018_1818);
        assert_eq!(theme.panel, 0x001b_1c1e);
        assert_eq!(theme.accent, 0x0066_aaff);
        assert_eq!(theme.chip_selected, blend(theme.chip, 0x0066_aaff, 0.22));
    }
}
