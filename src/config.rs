// SPDX-License-Identifier: AGPL-3.0-only
//! User configuration.
//!
//! The file lives at `$XDG_CONFIG_HOME/mureader/config.toml` (usually
//! `~/.config/mureader/config.toml`). `MUREADER_CONFIG` overrides
//! that path. A default file is written on first launch when the path is
//! missing.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const DEFAULT_CONFIG: &str = include_str!("../config.toml");

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub animation: AnimationConfig,
    pub reflowable: ReflowableConfig,
    pub perfect_palette: PerfectPaletteConfig,
    pub progress: ProgressConfig,
    pub keys: BTreeMap<String, KeySpec>,
    #[serde(skip)]
    base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    /// Page-turn slide duration in milliseconds.
    pub page_turn_ms: u64,
    /// Delay before re-rendering after the window stops resizing, in
    /// milliseconds. The page is scaled immediately while dragging.
    pub window_fit_ms: u64,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            page_turn_ms: 320,
            window_fit_ms: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReflowableConfig {
    pub css: Option<String>,
    pub css_file: Option<String>,
    /// Reflow plain text with the Unicode line-break pass before MuPDF
    /// renders it. This is a TXT-only subset, not an EPUB layout engine.
    pub pretty: bool,
}

impl Default for ReflowableConfig {
    fn default() -> Self {
        Self {
            css: None,
            css_file: None,
            pretty: true,
        }
    }
}

/// Optional eye-comfort palette ported from the `perfect-palette` helper in
/// the user's Emacs configuration. It tunes the existing theme instead of
/// replacing it, and only contributes CSS while `enable` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PerfectPaletteConfig {
    pub enable: bool,
    /// Number of colors to generate (`p`).
    pub colors: usize,
    /// Hue step (`q`); the actual step is `q / colors` turns.
    pub step: usize,
    /// Hue used when the existing theme is neutral, as a fraction of a turn.
    /// A tinted accent, window, or text color anchors the palette instead.
    pub hue_start: f32,
    /// Saturation in percent for the generated palette colors.
    pub saturation: f32,
    /// Lightness in percent for the generated palette colors.
    pub lightness: f32,
    /// Use HPLuv (`true`) or ordinary HSL (`false`).
    pub hpluv: bool,
}

impl Default for PerfectPaletteConfig {
    fn default() -> Self {
        let options = crate::palette::Options::default();
        Self {
            enable: false,
            colors: options.colors,
            step: options.step,
            hue_start: options.hue_start,
            saturation: options.saturation,
            lightness: options.lightness,
            hpluv: options.hpluv,
        }
    }
}

impl PerfectPaletteConfig {
    pub(crate) fn options(&self) -> crate::palette::Options {
        crate::palette::Options {
            colors: self.colors,
            step: self.step,
            hue_start: self.hue_start,
            saturation: self.saturation,
            lightness: self.lightness,
            hpluv: self.hpluv,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProgressConfig {
    /// How many documents keep a last-read page. The newest entries win.
    /// Zero disables reading progress.
    pub max_documents: usize,
}

impl Default for ProgressConfig {
    fn default() -> Self {
        Self { max_documents: 100 }
    }
}

impl ReflowableConfig {
    pub fn css(&self) -> Option<&str> {
        non_empty(self.css.as_deref())
    }

    pub fn css_file(&self) -> Option<&str> {
        non_empty(self.css_file.as_deref())
    }
}

impl Config {
    /// The first `font-family` declared in the reflowable CSS. UI text and
    /// document text share this primary face; glyph fallback remains shared.
    pub fn font_family(&self) -> Option<String> {
        let css = self.reflowable_css()?;
        first_css_font_family(&css)
    }

    /// Combine inline CSS and an optional CSS file. A relative file path is
    /// resolved against the config file's directory. The returned stylesheet is
    /// exactly the user's CSS; the optional palette is composed by the caller.
    pub fn reflowable_css(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(css) = self.reflowable.css() {
            parts.push(css.to_string());
        }

        if let Some(path) = self.reflowable.css_file() {
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else if let Some(base) = self.base_dir.as_ref() {
                base.join(path)
            } else {
                path.to_path_buf()
            };
            match fs::read_to_string(&path) {
                Ok(css) => parts.push(css),
                Err(error) => eprintln!(
                    "MuReader: cannot read reflowable CSS {}: {error}",
                    path.display()
                ),
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    pub fn entries(&self) -> Vec<&str> {
        match self {
            Self::One(value) => vec![value.as_str()],
            Self::Many(values) => values.iter().map(String::as_str).collect(),
        }
    }
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(&path, DEFAULT_CONFIG) {
            Ok(()) => eprintln!("MuReader: wrote default config to {}", path.display()),
            Err(error) => eprintln!("MuReader: cannot create config {}: {error}", path.display()),
        }
        return Config::default();
    }

    match fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(mut config) => {
                config.base_dir = path.parent().map(Path::to_path_buf);
                config
            }
            Err(error) => {
                eprintln!(
                    "MuReader: invalid config {}: {error}; using defaults",
                    path.display()
                );
                Config::default()
            }
        },
        Err(error) => {
            eprintln!(
                "MuReader: cannot read config {}: {error}; using defaults",
                path.display()
            );
            Config::default()
        }
    }
}

pub(crate) fn strip_css_comments(css: &str) -> String {
    let mut result = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("*/") {
            Some(end) => rest = &after[end + 2..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

fn is_css_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') || !ch.is_ascii()
}

pub(crate) fn strip_font_face_blocks(css: &str) -> String {
    let lower = css.to_ascii_lowercase();
    let mut result = String::with_capacity(css.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(relative) = lower[search..].find("@font-face") {
        let start = search + relative;
        if css[..start]
            .chars()
            .next_back()
            .is_some_and(is_css_ident_char)
        {
            search = start + 1;
            continue;
        }
        let Some(open_relative) = css[start..].find('{') else {
            break;
        };
        let open = start + open_relative;
        let mut depth = 0usize;
        let mut end = None;
        for (offset, ch) in css[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(open + offset + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(end) => {
                result.push_str(&css[copied..start]);
                copied = end;
                search = end;
            }
            None => break,
        }
    }
    result.push_str(&css[copied..]);
    result
}

fn first_css_font_family(css: &str) -> Option<String> {
    // `@font-face` names the source font, not necessarily the body face. The
    // UI should follow the family the document actually asks for.
    let css = strip_font_face_blocks(&strip_css_comments(css));
    let lower = css.to_ascii_lowercase();
    let mut search = 0usize;
    while let Some(relative) = lower[search..].find("font-family") {
        let index = search + relative;
        if css[..index]
            .chars()
            .next_back()
            .is_some_and(is_css_ident_char)
        {
            search = index + 1;
            continue;
        }
        let after = &lower[index + "font-family".len()..];
        let Some(colon) = after.find(':') else {
            break;
        };
        // A colon after a declaration boundary belongs to a later property.
        if after[..colon].contains([';', '{', '}']) {
            search = index + 1;
            continue;
        }
        let value_start = index + "font-family".len() + colon + 1;
        let value_lower = &lower[value_start..];
        let end = value_lower.find([';', '}']).unwrap_or(value_lower.len());
        let important = value_lower[..end].find("!important").unwrap_or(end);
        let value = css[value_start..value_start + end]
            .get(..important)
            .unwrap_or("")
            .trim();
        let first = value.split(',').next().unwrap_or(value).trim();
        let first = first.trim_matches(|ch| matches!(ch, '\'' | '"')).trim();
        if !first.is_empty() && !first.to_ascii_lowercase().starts_with("var(") {
            return Some(first.to_string());
        }
        search = value_start + end;
    }
    None
}

fn config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("MUREADER_CONFIG") {
        return Some(PathBuf::from(path));
    }

    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("mureader").join("config.toml"))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let serial = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mureader-{name}-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_key_specs() {
        let config: Config = toml::from_str(
            r#"
[keys]
quit = ["q", "Q"]
next_page = "n"
"#,
        )
        .unwrap();

        assert_eq!(config.keys["quit"].entries(), vec!["q", "Q"]);
        assert_eq!(config.keys["next_page"].entries(), vec!["n"]);
    }

    #[test]
    fn default_config_parses() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.animation.page_turn_ms, 320);
        assert_eq!(config.animation.window_fit_ms, 120);
        assert_eq!(config.reflowable.css(), None);
        assert_eq!(config.reflowable.css_file(), None);
        assert!(config.reflowable.pretty);
        assert_eq!(config.progress.max_documents, 100);
        assert!(!config.perfect_palette.enable);
        assert_eq!(config.perfect_palette.colors, 9);
        assert_eq!(config.perfect_palette.step, 4);
        assert_eq!(config.perfect_palette.hue_start, 0.58);
        assert_eq!(config.perfect_palette.saturation, 45.0);
        assert_eq!(config.perfect_palette.lightness, 65.0);
        assert!(config.perfect_palette.hpluv);
        assert!(config.keys.contains_key("quit"));
        assert!(config.keys.contains_key("outline"));
        assert_eq!(config.keys["toggle_spread"].entries(), vec!["s"]);
    }

    #[test]
    fn animation_durations_are_configurable() {
        let config: Config = toml::from_str(
            r#"
[animation]
page_turn_ms = 500
window_fit_ms = 80
"#,
        )
        .unwrap();
        assert_eq!(config.animation.page_turn_ms, 500);
        assert_eq!(config.animation.window_fit_ms, 80);
    }

    #[test]
    fn pretty_text_can_be_disabled() {
        let config: Config = toml::from_str(
            r#"
[reflowable]
pretty = false
"#,
        )
        .unwrap();
        assert!(!config.reflowable.pretty);
    }

    #[test]
    fn progress_document_limit_is_configurable() {
        let config: Config = toml::from_str(
            r#"
[progress]
max_documents = 25
"#,
        )
        .unwrap();
        assert_eq!(config.progress.max_documents, 25);

        let config: Config = toml::from_str(
            r#"
[progress]
max_documents = 0
"#,
        )
        .unwrap();
        assert_eq!(config.progress.max_documents, 0);
    }

    #[test]
    fn perfect_palette_does_not_alter_the_user_stylesheet() {
        let mut config = Config::default();
        config.reflowable.css = Some("body { color: red; }".to_string());
        assert_eq!(
            config.reflowable_css().as_deref(),
            Some("body { color: red; }")
        );

        config.perfect_palette.enable = true;
        assert_eq!(
            config.reflowable_css().as_deref(),
            Some("body { color: red; }"),
            "the palette is composed after the user's CSS, not inside it"
        );
    }

    #[test]
    fn perfect_palette_parameters_are_configurable() {
        let config: Config = toml::from_str(
            r#"
[perfect_palette]
enable = true
colors = 5
step = 2
hue_start = 0.1
saturation = 80
lightness = 30
hpluv = false
"#,
        )
        .unwrap();
        assert!(config.perfect_palette.enable);
        assert_eq!(config.perfect_palette.colors, 5);
        assert_eq!(config.perfect_palette.step, 2);
        assert_eq!(config.perfect_palette.hue_start, 0.1);
        assert_eq!(config.perfect_palette.saturation, 80.0);
        assert_eq!(config.perfect_palette.lightness, 30.0);
        assert!(!config.perfect_palette.hpluv);
        let options = config.perfect_palette.options();
        assert_eq!(options.colors, 5);
        assert_eq!(options.step, 2);
        assert_eq!(options.hue_start, 0.1);
        assert_eq!(options.saturation, 80.0);
        assert_eq!(options.lightness, 30.0);
        assert!(!options.hpluv);
        assert!(config.reflowable_css().is_none());
    }

    #[test]
    fn combines_inline_and_file_css() {
        let dir = test_dir("css-config");
        std::fs::write(dir.join("style.css"), "body { color: red; }").unwrap();

        let mut config = Config::default();
        config.reflowable.css = Some("html { background: black; }".to_string());
        config.reflowable.css_file = Some("style.css".to_string());
        config.base_dir = Some(dir.clone());

        let css = config.reflowable_css().unwrap();
        assert!(css.contains("background: black"));
        assert!(css.contains("color: red"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_first_css_font_family() {
        let family = first_css_font_family(
            r#"
@font-face { font-family: "Embedded Face"; src: url(x); }
body { color: red; font-family: "FangSongCode", serif !important; }
"#,
        );
        assert_eq!(family.as_deref(), Some("FangSongCode"));
        assert_eq!(
            first_css_font_family("body { font-family: 'Noto Serif', serif; }").as_deref(),
            Some("Noto Serif")
        );
        assert_eq!(first_css_font_family("body { color: red; }"), None);
    }

    #[test]
    fn css_font_family_ignores_comments_face_blocks_and_case() {
        let family = first_css_font_family(
            r#"
/* font-family: "Commented Out"; */
@FONT-FACE { font-family: "Embedded Face"; src: url(x); }
body {
    --font-family: "Custom Property";
    FONT-FAMILY: 'Real Face', serif !IMPORTANT;
}
"#,
        );
        assert_eq!(family.as_deref(), Some("Real Face"));
        assert_eq!(
            first_css_font_family("body { --font-family: 'No'; font-family: 'Yes'; }").as_deref(),
            Some("Yes")
        );
    }
}
