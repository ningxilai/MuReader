//! Configurable normal-mode keybindings.

use std::collections::BTreeMap;

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::config::KeySpec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Quit,
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    ScrollDown,
    ScrollUp,
    ScrollDownScreen,
    ScrollUpScreen,
    ScrollLeft,
    ScrollRight,
    ScrollLeftMost,
    ScrollRightMost,
    GotoPage,
    Outline,
    SearchPage,
    SearchBook,
    ZoomIn,
    ZoomOut,
    FontIncrease,
    FontDecrease,
    ResetView,
    RotateRight,
    RotateLeft,
    Copy,
}

impl Action {
    fn from_name(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase().replace('-', "_");
        Some(match name.as_str() {
            "quit" => Self::Quit,
            "next_page" => Self::NextPage,
            "prev_page" => Self::PrevPage,
            "first_page" => Self::FirstPage,
            "last_page" => Self::LastPage,
            "scroll_down" => Self::ScrollDown,
            "scroll_up" => Self::ScrollUp,
            "scroll_down_screen" => Self::ScrollDownScreen,
            "scroll_up_screen" => Self::ScrollUpScreen,
            "scroll_left" => Self::ScrollLeft,
            "scroll_right" => Self::ScrollRight,
            "scroll_left_most" => Self::ScrollLeftMost,
            "scroll_right_most" => Self::ScrollRightMost,
            "goto_page" => Self::GotoPage,
            "outline" => Self::Outline,
            "search_page" => Self::SearchPage,
            "search_book" => Self::SearchBook,
            "zoom_in" => Self::ZoomIn,
            "zoom_out" => Self::ZoomOut,
            "font_increase" => Self::FontIncrease,
            "font_decrease" => Self::FontDecrease,
            "reset_view" => Self::ResetView,
            "rotate_right" => Self::RotateRight,
            "rotate_left" => Self::RotateLeft,
            "copy" => Self::Copy,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum KeyCode {
    Char(char),
    Named(NamedKey),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Chord {
    key: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl Chord {
    fn from_event(event: &KeyEvent, modifiers: ModifiersState) -> Option<Self> {
        let key = match event.logical_key.as_ref() {
            Key::Character(ch) => {
                let mut chars = ch.chars();
                let value = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if value == ' ' {
                    KeyCode::Named(NamedKey::Space)
                } else {
                    KeyCode::Char(value)
                }
            }
            Key::Named(named) => KeyCode::Named(named),
            _ => return None,
        };
        // Character keys already encode Shift in their case. Keeping the
        // modifier here would make "Q" and "S-q" behave differently for no
        // reason.
        let shift = match key {
            KeyCode::Char(_) => false,
            KeyCode::Named(_) => modifiers.shift_key(),
        };
        Some(Self {
            key,
            ctrl: modifiers.control_key(),
            alt: modifiers.alt_key(),
            shift,
        })
    }

    fn parse(spec: &str) -> Result<Self, String> {
        let mut rest = spec.trim();
        if rest.is_empty() {
            return Err("empty key".to_string());
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        while let Some((prefix, tail)) = rest.split_once('-') {
            if tail.is_empty() {
                break;
            }
            match prefix.to_ascii_lowercase().as_str() {
                "c" | "ctrl" | "control" => ctrl = true,
                "m" | "alt" | "meta" => alt = true,
                "s" | "shift" => shift = true,
                _ => break,
            }
            rest = tail;
        }

        let mut key = if rest.chars().count() == 1 {
            KeyCode::Char(rest.chars().next().expect("one character"))
        } else {
            KeyCode::Named(named_key(rest)?)
        };

        // Shift is part of the character itself for printable keys.
        if shift {
            if let KeyCode::Char(ch) = key {
                if ch.is_ascii_alphabetic() {
                    key = KeyCode::Char(ch.to_ascii_uppercase());
                }
            }
        }
        let shift = match key {
            KeyCode::Char(_) => false,
            KeyCode::Named(_) => shift,
        };

        Ok(Self {
            key,
            ctrl,
            alt,
            shift,
        })
    }
}

fn named_key(name: &str) -> Result<NamedKey, String> {
    let name = name.to_ascii_lowercase();
    Ok(match name.as_str() {
        "space" => NamedKey::Space,
        "return" | "enter" => NamedKey::Enter,
        "escape" | "esc" => NamedKey::Escape,
        "tab" => NamedKey::Tab,
        "backspace" => NamedKey::Backspace,
        "delete" | "del" => NamedKey::Delete,
        "insert" => NamedKey::Insert,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "pgup" => NamedKey::PageUp,
        "pagedown" | "pgdn" => NamedKey::PageDown,
        "up" | "arrowup" => NamedKey::ArrowUp,
        "down" | "arrowdown" => NamedKey::ArrowDown,
        "left" | "arrowleft" => NamedKey::ArrowLeft,
        "right" | "arrowright" => NamedKey::ArrowRight,
        _ => return Err(format!("unknown key name {name:?}")),
    })
}

struct Binding {
    keys: Vec<Chord>,
    action: Action,
}

pub enum Resolution {
    Action(Action),
    Pending,
    Ignored,
}

pub struct Keymap {
    bindings: Vec<Binding>,
    pending: Vec<Chord>,
}

impl Keymap {
    pub fn new(overrides: &BTreeMap<String, KeySpec>) -> Self {
        let mut keymap = Self {
            bindings: Vec::new(),
            pending: Vec::new(),
        };

        for (action, keys) in DEFAULT_BINDINGS {
            let keys = parse_sequence(keys).expect("valid built-in key binding");
            keymap.bindings.push(Binding {
                keys,
                action: *action,
            });
        }

        keymap.apply_overrides(overrides);
        keymap
    }

    pub fn handle(&mut self, event: &KeyEvent, modifiers: ModifiersState) -> Resolution {
        let Some(chord) = Chord::from_event(event, modifiers) else {
            self.pending.clear();
            return Resolution::Ignored;
        };

        let mut candidate = std::mem::take(&mut self.pending);
        candidate.push(chord);
        if let Some(action) = self.exact(&candidate) {
            return Resolution::Action(action);
        }
        if self.is_prefix(&candidate) {
            self.pending = candidate;
            return Resolution::Pending;
        }

        Resolution::Ignored
    }

    pub fn reset(&mut self) {
        self.pending.clear();
    }

    fn exact(&self, keys: &[Chord]) -> Option<Action> {
        self.bindings
            .iter()
            .find(|binding| binding.keys == keys)
            .map(|binding| binding.action)
    }

    fn is_prefix(&self, keys: &[Chord]) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.keys.len() > keys.len() && binding.keys.starts_with(keys))
    }

    fn apply_overrides(&mut self, overrides: &BTreeMap<String, KeySpec>) {
        for (name, spec) in overrides {
            let Some(action) = Action::from_name(name) else {
                eprintln!("MuReader: unknown keybinding action {name:?}");
                continue;
            };

            let mut sequences = Vec::new();
            let mut valid = true;
            for entry in spec.entries() {
                match parse_sequence(entry) {
                    Ok(keys) => {
                        if !keys.is_empty() {
                            sequences.push(keys);
                        }
                    }
                    Err(error) => {
                        eprintln!("MuReader: invalid keybinding for {name}: {error}");
                        valid = false;
                        break;
                    }
                }
            }
            if !valid {
                continue;
            }

            self.bindings.retain(|binding| binding.action != action);
            for keys in sequences {
                self.bindings.push(Binding { keys, action });
            }
        }
    }
}

fn parse_sequence(spec: &str) -> Result<Vec<Chord>, String> {
    spec.split_whitespace().map(Chord::parse).collect()
}

const DEFAULT_BINDINGS: &[(Action, &str)] = &[
    (Action::Quit, "q"),
    (Action::Quit, "Q"),
    (Action::Quit, "Escape"),
    (Action::NextPage, "n"),
    (Action::PrevPage, "p"),
    (Action::FirstPage, "g"),
    (Action::FirstPage, "Home"),
    (Action::FirstPage, "M-<"),
    (Action::LastPage, "G"),
    (Action::LastPage, "End"),
    (Action::LastPage, "M->"),
    (Action::ScrollDown, "Down"),
    (Action::ScrollDown, "PageDown"),
    (Action::ScrollDown, "Space"),
    (Action::ScrollDown, "C-n"),
    (Action::ScrollUp, "Up"),
    (Action::ScrollUp, "PageUp"),
    (Action::ScrollUp, "S-Space"),
    (Action::ScrollUp, "Delete"),
    (Action::ScrollUp, "C-p"),
    (Action::ScrollDownScreen, "C-v"),
    (Action::ScrollUpScreen, "M-v"),
    (Action::ScrollLeft, "Left"),
    (Action::ScrollLeft, "C-b"),
    (Action::ScrollRight, "Right"),
    (Action::ScrollRight, "C-f"),
    (Action::ScrollLeftMost, "C-a"),
    (Action::ScrollRightMost, "C-e"),
    (Action::GotoPage, "M-g g"),
    (Action::Outline, "o"),
    (Action::SearchPage, "C-s"),
    (Action::SearchBook, "M-s"),
    (Action::ZoomIn, "+"),
    (Action::ZoomOut, "-"),
    (Action::FontIncrease, "C-="),
    (Action::FontDecrease, "C--"),
    (Action::ResetView, "0"),
    (Action::RotateRight, "r"),
    (Action::RotateLeft, "l"),
    (Action::Copy, "M-w"),
    (Action::Copy, "C-w"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_sequences_and_symbols() {
        let sequence = parse_sequence("M-g g").unwrap();
        assert_eq!(sequence.len(), 2);
        assert!(sequence[0].alt);
        assert_eq!(sequence[0].key, KeyCode::Char('g'));
        assert_eq!(sequence[1].key, KeyCode::Char('g'));

        let ctrl_s = Chord::parse("C-s").unwrap();
        assert!(ctrl_s.ctrl);
        assert_eq!(ctrl_s.key, KeyCode::Char('s'));

        let shift_space = Chord::parse("S-Space").unwrap();
        assert!(shift_space.shift);
        assert_eq!(shift_space.key, KeyCode::Named(NamedKey::Space));

        assert_eq!(Chord::parse("+").unwrap().key, KeyCode::Char('+'));
        assert_eq!(Chord::parse("-").unwrap().key, KeyCode::Char('-'));
        assert_eq!(Chord::parse("C--").unwrap().key, KeyCode::Char('-'));
        assert_eq!(Chord::parse("Q").unwrap().key, KeyCode::Char('Q'));
        assert!(Chord::parse("Bogus").is_err());
    }

    fn assert_no_collisions(keymap: &Keymap) {
        for (index, first) in keymap.bindings.iter().enumerate() {
            for second in &keymap.bindings[index + 1..] {
                assert_ne!(
                    first.keys, second.keys,
                    "duplicate binding for {:?} and {:?}",
                    first.action, second.action
                );
            }
        }
    }

    #[test]
    fn default_bindings_do_not_collide() {
        assert_no_collisions(&Keymap::new(&BTreeMap::new()));
    }

    #[test]
    fn goto_page_is_a_two_key_sequence() {
        let keymap = Keymap::new(&BTreeMap::new());
        let first = Chord::parse("M-g").unwrap();
        let second = Chord::parse("g").unwrap();
        assert!(keymap.is_prefix(&[first]));
        assert_eq!(keymap.exact(&[first, second]), Some(Action::GotoPage));
    }

    #[test]
    fn default_config_has_valid_bindings() {
        let config: crate::config::Config = toml::from_str(include_str!("../config.toml")).unwrap();
        assert_no_collisions(&Keymap::new(&config.keys));
    }

    #[test]
    fn overrides_replace_defaults_for_the_action() {
        let mut overrides = BTreeMap::new();
        overrides.insert("next_page".to_string(), KeySpec::One("C-j".to_string()));
        let keymap = Keymap::new(&overrides);

        let old = Keymap::new(&BTreeMap::new());
        assert_eq!(
            old.exact(&parse_sequence("n").unwrap()),
            Some(Action::NextPage)
        );
        assert_eq!(old.exact(&parse_sequence("C-j").unwrap()), None);
        assert_eq!(
            keymap.exact(&parse_sequence("C-j").unwrap()),
            Some(Action::NextPage)
        );
        assert_eq!(keymap.exact(&parse_sequence("n").unwrap()), None);
    }

    #[test]
    fn empty_override_disables_action() {
        let mut overrides = BTreeMap::new();
        overrides.insert("quit".to_string(), KeySpec::Many(Vec::new()));
        let keymap = Keymap::new(&overrides);
        assert_eq!(keymap.exact(&parse_sequence("q").unwrap()), None);
        assert_eq!(keymap.exact(&parse_sequence("Escape").unwrap()), None);
    }
}
