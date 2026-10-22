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
    /// resolved against the config file's directory.
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

fn strip_font_face_blocks(css: &str) -> String {
    let mut result = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(index) = rest.find("@font-face") {
        result.push_str(&rest[..index]);
        let after = &rest[index..];
        let Some(open) = after.find('{') else {
            return result;
        };
        let mut depth = 0usize;
        let mut end = None;
        for (offset, ch) in after[open..].char_indices() {
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
            Some(end) => rest = &after[end..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

fn first_css_font_family(css: &str) -> Option<String> {
    // `@font-face` names the source font, not necessarily the body face. The
    // UI should follow the family the document actually asks for.
    let css = strip_font_face_blocks(css);
    let mut rest = css.as_str();
    while let Some(index) = rest.find("font-family") {
        let after = &rest[index + "font-family".len()..];
        let Some(colon) = after.find(':') else {
            break;
        };
        let value = &after[colon + 1..];
        let end = value.find([';', '}']).unwrap_or(value.len());
        let value = value[..end].trim();
        let first = value.split(',').next().unwrap_or(value).trim();
        let first = first
            .trim_matches(|ch| matches!(ch, '\'' | '"'))
            .trim_end_matches("!important")
            .trim();
        if !first.is_empty() && !first.starts_with("var(") {
            return Some(first.to_string());
        }
        rest = &after[colon + 1 + end..];
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
        assert!(config.keys.contains_key("quit"));
        assert!(config.keys.contains_key("goto_page"));
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
    fn combines_inline_and_file_css() {
        let dir = std::env::temp_dir().join("mureader-css-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
}
