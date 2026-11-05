// SPDX-License-Identifier: AGPL-3.0-only
//! Configurable normal-mode keybindings.

use std::collections::BTreeMap;

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::config::KeySpec;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

    fn label(&self) -> String {
        let mut label = String::new();
        if self.ctrl {
            label.push_str("C-");
        }
        if self.alt {
            label.push_str("M-");
        }
        if self.shift {
            label.push_str("S-");
        }
        match self.key {
            KeyCode::Char(ch) => label.push(ch),
            KeyCode::Named(named) => match named_key_label(named) {
                Some(named) => label.push_str(named),
                None => label.push_str(&format!("{named:?}")),
            },
        }
        label
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

fn named_key_label(name: NamedKey) -> Option<&'static str> {
    Some(match name {
        NamedKey::Space => "Space",
        NamedKey::Enter => "Enter",
        NamedKey::Escape => "Esc",
        NamedKey::Tab => "Tab",
        NamedKey::Backspace => "Backspace",
        NamedKey::Delete => "Delete",
        NamedKey::Insert => "Insert",
        NamedKey::Home => "Home",
        NamedKey::End => "End",
        NamedKey::PageUp => "PageUp",
        NamedKey::PageDown => "PageDown",
        NamedKey::ArrowUp => "Up",
        NamedKey::ArrowDown => "Down",
        NamedKey::ArrowLeft => "Left",
        NamedKey::ArrowRight => "Right",
        _ => return None,
    })
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

    /// Build the shortcut table for the command panel. Only `actions` are
    /// exposed, and bindings that contain a reserved panel chord are removed
    /// so the panel and the reader cannot claim the same key.
    pub fn menu_keymap(&self, actions: &[Action], reserved: &[&str]) -> MenuKeymap {
        let reserved: Vec<Chord> = reserved
            .iter()
            .filter_map(|spec| {
                let Ok(mut keys) = parse_sequence(spec) else {
                    debug_assert!(false, "invalid menu-reserved key {spec:?}");
                    return None;
                };
                debug_assert_eq!(keys.len(), 1, "menu-reserved keys are single chords");
                (keys.len() == 1).then(|| keys.remove(0))
            })
            .collect();

        let mut bound = Vec::new();
        for binding in &self.bindings {
            if !bound.contains(&binding.action) {
                bound.push(binding.action);
            }
        }
        let bindings = self
            .bindings
            .iter()
            .filter(|binding| actions.contains(&binding.action))
            .filter(|binding| !binding.keys.iter().any(|chord| reserved.contains(chord)))
            .map(|binding| MenuBinding {
                keys: binding.keys.clone(),
                action: binding.action,
            })
            .collect();

        MenuKeymap {
            bindings,
            bound,
            pending: Vec::new(),
        }
    }
}

struct MenuBinding {
    keys: Vec<Chord>,
    action: Action,
}

#[derive(Debug, Eq, PartialEq)]
pub struct MenuCandidate {
    /// The chord that continues the pending prefix.
    pub key: String,
    pub action: Action,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MenuResolution {
    Action(Action),
    Pending,
    /// The rejected sequence, for the panel footer.
    Ignored(String),
}

/// which-key-style state for the command panel.
///
/// Every transition either resolves to an action, keeps a non-empty prefix
/// with at least one continuation, or clears the prefix. There is no timeout
/// and no state that outlives an invalid press, so a stuck prefix is
/// impossible: the next key either continues, cancels, or is rejected.
pub struct MenuKeymap {
    bindings: Vec<MenuBinding>,
    bound: Vec<Action>,
    pending: Vec<Chord>,
}

impl MenuKeymap {
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the action had any binding before panel-reserved keys were
    /// removed. An action disabled in the config has no binding at all.
    pub fn has_binding(&self, action: Action) -> bool {
        self.bound.contains(&action)
    }

    pub fn pop_pending(&mut self) -> bool {
        self.pending.pop().is_some()
    }

    pub fn pending_label(&self) -> Option<String> {
        (!self.pending.is_empty()).then(|| format_sequence(&self.pending))
    }

    /// The key shown in the panel for an action. The first effective binding
    /// wins, so the panel follows config order without inventing shortcuts.
    pub fn primary_label(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| format_sequence(&binding.keys))
    }

    /// Continuations for the current prefix. The key column shows the next
    /// chord to press, not the whole sequence, which is what which-key does.
    pub fn candidates(&self) -> Vec<MenuCandidate> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let mut candidates: Vec<MenuCandidate> = Vec::new();
        for binding in &self.bindings {
            if binding.keys.len() <= self.pending.len() || !binding.keys.starts_with(&self.pending)
            {
                continue;
            }
            let Some(next) = binding.keys.get(self.pending.len()) else {
                continue;
            };
            let candidate = MenuCandidate {
                key: next.label(),
                action: binding.action,
            };
            if !candidates.iter().any(|existing| {
                existing.key == candidate.key && existing.action == candidate.action
            }) {
                candidates.push(candidate);
            }
        }
        candidates
    }

    pub fn press(&mut self, event: &KeyEvent, modifiers: ModifiersState) -> MenuResolution {
        let Some(chord) = Chord::from_event(event, modifiers) else {
            let attempted = self.pending_label().unwrap_or_default();
            self.pending.clear();
            return MenuResolution::Ignored(attempted);
        };
        self.press_chord(chord)
    }

    fn press_chord(&mut self, chord: Chord) -> MenuResolution {
        let mut candidate = std::mem::take(&mut self.pending);
        candidate.push(chord);
        if let Some(action) = self.exact(&candidate) {
            return MenuResolution::Action(action);
        }
        if self.is_prefix(&candidate) {
            self.pending = candidate;
            return MenuResolution::Pending;
        }
        MenuResolution::Ignored(format_sequence(&candidate))
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
}

fn format_sequence(keys: &[Chord]) -> String {
    keys.iter().map(Chord::label).collect::<Vec<_>>().join(" ")
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
    fn outline_is_bound_to_o() {
        let keymap = Keymap::new(&BTreeMap::new());
        let key = Chord::parse("o").unwrap();
        assert_eq!(keymap.exact(&[key]), Some(Action::Outline));
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

    const MENU_TEST_ACTIONS: &[Action] = &[
        Action::NextPage,
        Action::PrevPage,
        Action::FirstPage,
        Action::Outline,
        Action::SearchPage,
        Action::SearchBook,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::FontIncrease,
        Action::FontDecrease,
        Action::Copy,
        Action::Quit,
    ];

    const MENU_TEST_RESERVED: &[&str] = &[
        "Escape",
        "Enter",
        "Up",
        "Down",
        "PageUp",
        "PageDown",
        "Home",
        "End",
        "Backspace",
        "C-g",
        "q",
    ];

    fn menu_with(overrides: BTreeMap<String, KeySpec>) -> MenuKeymap {
        Keymap::new(&overrides).menu_keymap(MENU_TEST_ACTIONS, MENU_TEST_RESERVED)
    }

    #[test]
    fn menu_shows_effective_combinations() {
        let menu = menu_with(BTreeMap::new());
        assert_eq!(
            menu.primary_label(Action::SearchPage).as_deref(),
            Some("C-s")
        );
        assert_eq!(
            menu.primary_label(Action::SearchBook).as_deref(),
            Some("M-s")
        );
        assert_eq!(
            menu.primary_label(Action::FontIncrease).as_deref(),
            Some("C-=")
        );
        assert_eq!(
            menu.primary_label(Action::FontDecrease).as_deref(),
            Some("C--")
        );
        assert_eq!(menu.primary_label(Action::Copy).as_deref(), Some("M-w"));
        assert_eq!(menu.primary_label(Action::ZoomIn).as_deref(), Some("+"));
        assert_eq!(menu.primary_label(Action::FirstPage).as_deref(), Some("g"));
        assert_eq!(menu.primary_label(Action::Quit).as_deref(), Some("Q"));
    }

    #[test]
    fn menu_uses_overridden_bindings_and_hides_disabled_actions() {
        let mut overrides = BTreeMap::new();
        overrides.insert("next_page".to_string(), KeySpec::One("C-j".to_string()));
        overrides.insert("outline".to_string(), KeySpec::Many(Vec::new()));
        let mut menu = menu_with(overrides);

        assert_eq!(menu.primary_label(Action::NextPage).as_deref(), Some("C-j"));
        assert_eq!(
            menu.press_chord(Chord::parse("C-j").unwrap()),
            MenuResolution::Action(Action::NextPage)
        );
        assert!(!menu.has_pending());
        assert!(!menu.has_binding(Action::Outline));
        assert_eq!(menu.primary_label(Action::Outline), None);
    }

    #[test]
    fn menu_prefix_waits_then_resolves_or_clears() {
        let mut overrides = BTreeMap::new();
        overrides.insert("next_page".to_string(), KeySpec::One("M-g g".to_string()));
        overrides.insert("prev_page".to_string(), KeySpec::One("M-g G".to_string()));
        let mut menu = menu_with(overrides);

        assert_eq!(
            menu.press_chord(Chord::parse("M-g").unwrap()),
            MenuResolution::Pending
        );
        assert!(menu.has_pending());
        assert_eq!(menu.pending_label().as_deref(), Some("M-g"));
        let candidates = menu.candidates();
        assert!(candidates.contains(&MenuCandidate {
            key: "g".to_string(),
            action: Action::NextPage,
        }));
        assert!(candidates.contains(&MenuCandidate {
            key: "G".to_string(),
            action: Action::PrevPage,
        }));

        assert_eq!(
            menu.press_chord(Chord::parse("g").unwrap()),
            MenuResolution::Action(Action::NextPage)
        );
        assert!(!menu.has_pending());

        assert_eq!(
            menu.press_chord(Chord::parse("M-g").unwrap()),
            MenuResolution::Pending
        );
        assert_eq!(
            menu.press_chord(Chord::parse("z").unwrap()),
            MenuResolution::Ignored("M-g z".to_string())
        );
        assert!(
            !menu.has_pending(),
            "a rejected chord must clear the prefix"
        );

        assert_eq!(
            menu.press_chord(Chord::parse("M-g").unwrap()),
            MenuResolution::Pending
        );
        assert!(menu.pop_pending());
        assert!(!menu.has_pending());
    }

    #[test]
    fn menu_reserved_controls_cannot_be_shadowed() {
        let mut menu = menu_with(BTreeMap::new());
        assert_eq!(
            menu.press_chord(Chord::parse("Escape").unwrap()),
            MenuResolution::Ignored("Esc".to_string())
        );
        assert_eq!(
            menu.press_chord(Chord::parse("Home").unwrap()),
            MenuResolution::Ignored("Home".to_string())
        );
        assert!(!menu.has_pending());
    }

    #[test]
    fn menu_resolves_exact_bindings_before_longer_prefixes() {
        let mut overrides = BTreeMap::new();
        overrides.insert("next_page".to_string(), KeySpec::One("x".to_string()));
        overrides.insert("prev_page".to_string(), KeySpec::One("x y".to_string()));
        let mut menu = menu_with(overrides);
        assert_eq!(
            menu.press_chord(Chord::parse("x").unwrap()),
            MenuResolution::Action(Action::NextPage)
        );
        assert!(!menu.has_pending());
    }
}
