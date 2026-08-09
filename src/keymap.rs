//! User-rebindable keyboard shortcuts.
//!
//! Every shortcut the app dispatches goes through an [`Action`]. [`Keymap`]
//! maps actions to chords and is the single place that decides whether a key
//! press fires, so the Preferences UI, the menu labels and the ribbon tooltips
//! all agree with what actually happens.

use std::collections::HashMap;

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

/// Something the user can bind a key to.
///
/// Not everything the app listens for is here. Escape (cancel a gesture, leave
/// a text box, close the find bar), Enter in the find bar, arrow-key nudging
/// and space-to-pan stay fixed: they are modal, context-specific keys rather
/// than commands, and rebinding them would mostly break the modes they belong
/// to. Preferences says so rather than leaving the user to wonder.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Action {
    Open,
    Save,
    Print,
    Find,
    CloseDocument,
    Preferences,

    Undo,
    Redo,
    DeleteSelection,

    ZoomIn,
    ZoomOut,
    ZoomActual,
    ZoomFitWidth,
    ToggleChat,

    ToolSelect,
    ToolPan,
    ToolHighlight,
    ToolText,
    ToolRect,
    ToolEllipse,
    ToolLine,
    ToolArrow,
    ToolPen,
    ToolCloud,
    ToolPolygon,
    ToolPolyLine,
}

/// Where an action appears in the Preferences list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    File,
    Edit,
    View,
    Tools,
}

impl Category {
    pub const ALL: [Category; 4] = [Self::File, Self::Edit, Self::View, Self::Tools];

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Edit => "Edit",
            Self::View => "View",
            Self::Tools => "Tools",
        }
    }
}

impl Action {
    pub const ALL: [Action; 26] = [
        Self::Open,
        Self::Save,
        Self::Print,
        Self::Find,
        Self::CloseDocument,
        Self::Preferences,
        Self::Undo,
        Self::Redo,
        Self::DeleteSelection,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::ZoomActual,
        Self::ZoomFitWidth,
        Self::ToggleChat,
        Self::ToolSelect,
        Self::ToolPan,
        Self::ToolHighlight,
        Self::ToolText,
        Self::ToolRect,
        Self::ToolEllipse,
        Self::ToolLine,
        Self::ToolArrow,
        Self::ToolPen,
        Self::ToolCloud,
        Self::ToolPolygon,
        Self::ToolPolyLine,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open…",
            Self::Save => "Save",
            Self::Print => "Print…",
            Self::Find => "Find",
            Self::CloseDocument => "Close Document",
            Self::Preferences => "Preferences…",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::DeleteSelection => "Delete Selection",
            Self::ZoomIn => "Zoom In",
            Self::ZoomOut => "Zoom Out",
            Self::ZoomActual => "Actual Size",
            Self::ZoomFitWidth => "Fit Width",
            Self::ToggleChat => "Chat with Document",
            Self::ToolSelect => "Select",
            Self::ToolPan => "Pan",
            Self::ToolHighlight => "Highlight",
            Self::ToolText => "Text",
            Self::ToolRect => "Rectangle",
            Self::ToolEllipse => "Ellipse",
            Self::ToolLine => "Line",
            Self::ToolArrow => "Arrow",
            Self::ToolPen => "Pen",
            Self::ToolCloud => "Cloud",
            Self::ToolPolygon => "Polygon",
            Self::ToolPolyLine => "Polyline",
        }
    }

    pub fn category(self) -> Category {
        match self {
            Self::Open | Self::Save | Self::Print | Self::CloseDocument | Self::Preferences => {
                Category::File
            }
            Self::Undo | Self::Redo | Self::DeleteSelection | Self::Find => Category::Edit,
            Self::ZoomIn
            | Self::ZoomOut
            | Self::ZoomActual
            | Self::ZoomFitWidth
            | Self::ToggleChat => Category::View,
            _ => Category::Tools,
        }
    }

    pub fn default_shortcut(self) -> Option<KeyboardShortcut> {
        let cmd = Modifiers::COMMAND;
        let none = Modifiers::NONE;
        let shortcut = |m, k| Some(KeyboardShortcut::new(m, k));
        match self {
            Self::Open => shortcut(cmd, Key::O),
            Self::Save => shortcut(cmd, Key::S),
            Self::Print => shortcut(cmd, Key::P),
            Self::Find => shortcut(cmd, Key::F),
            Self::CloseDocument => shortcut(cmd, Key::W),
            Self::Preferences => shortcut(cmd, Key::Comma),
            Self::Undo => shortcut(cmd, Key::Z),
            Self::Redo => shortcut(cmd | Modifiers::SHIFT, Key::Z),
            Self::DeleteSelection => shortcut(none, Key::Delete),
            Self::ZoomIn => shortcut(cmd, Key::Equals),
            Self::ZoomOut => shortcut(cmd, Key::Minus),
            Self::ZoomActual => shortcut(cmd, Key::Num0),
            Self::ZoomFitWidth => shortcut(cmd, Key::Num9),
            Self::ToggleChat => shortcut(cmd | Modifiers::SHIFT, Key::C),
            Self::ToolSelect => shortcut(none, Key::V),
            // Pan has always been space-held, which stays; a tool key for it
            // is new and matches every other tool.
            Self::ToolPan => shortcut(none, Key::G),
            Self::ToolHighlight => shortcut(none, Key::H),
            Self::ToolText => shortcut(none, Key::T),
            Self::ToolRect => shortcut(none, Key::R),
            Self::ToolEllipse => shortcut(none, Key::O),
            Self::ToolLine => shortcut(none, Key::L),
            Self::ToolArrow => shortcut(none, Key::A),
            Self::ToolPen => shortcut(none, Key::P),
            Self::ToolCloud => shortcut(none, Key::C),
            Self::ToolPolygon => shortcut(none, Key::Y),
            // The polyline is the polygon left open, so it is the polygon key
            // with a shift on it -- the pairing every drawing app uses.
            Self::ToolPolyLine => shortcut(Modifiers::SHIFT, Key::Y),
        }
    }
}

/// A chord in a form that survives a round-trip through storage.
///
/// egui's own types are serializable, but their representation is not part of
/// its public contract; a key name is.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StoredBinding {
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub mac_cmd: bool,
    #[serde(default)]
    pub command: bool,
    pub key: String,
}

impl StoredBinding {
    fn from_shortcut(s: &KeyboardShortcut) -> Self {
        Self {
            alt: s.modifiers.alt,
            ctrl: s.modifiers.ctrl,
            shift: s.modifiers.shift,
            mac_cmd: s.modifiers.mac_cmd,
            command: s.modifiers.command,
            key: s.logical_key.name().to_owned(),
        }
    }

    fn to_shortcut(&self) -> Option<KeyboardShortcut> {
        Some(KeyboardShortcut::new(
            Modifiers {
                alt: self.alt,
                ctrl: self.ctrl,
                shift: self.shift,
                mac_cmd: self.mac_cmd,
                command: self.command,
            },
            Key::from_name(&self.key)?,
        ))
    }
}

/// What gets persisted: only the bindings that differ from the defaults, so
/// improving a default still reaches everyone who never changed it. A `None`
/// value is a deliberately unbound action.
#[derive(Default, Serialize, Deserialize)]
pub struct StoredKeymap(Vec<(Action, Option<StoredBinding>)>);

#[derive(Default, Clone)]
pub struct Keymap {
    overrides: HashMap<Action, Option<KeyboardShortcut>>,
}

impl Keymap {
    pub fn shortcut(&self, action: Action) -> Option<KeyboardShortcut> {
        match self.overrides.get(&action) {
            Some(over) => *over,
            None => action.default_shortcut(),
        }
    }

    pub fn is_default(&self, action: Action) -> bool {
        !self.overrides.contains_key(&action)
    }

    pub fn set(&mut self, action: Action, shortcut: Option<KeyboardShortcut>) {
        if shortcut == action.default_shortcut() {
            self.overrides.remove(&action);
        } else {
            self.overrides.insert(action, shortcut);
        }
    }

    pub fn reset(&mut self, action: Action) {
        self.overrides.remove(&action);
    }

    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    /// Other actions already bound to this chord.
    pub fn conflicts(&self, shortcut: KeyboardShortcut, ignoring: Action) -> Vec<Action> {
        Action::ALL
            .iter()
            .copied()
            .filter(|&a| a != ignoring && self.shortcut(a) == Some(shortcut))
            .collect()
    }

    /// Fire `action` if its chord was pressed, consuming the press.
    ///
    /// A chord with no modifiers is suppressed while a text field has the
    /// keyboard, so typing "highlight" in a text box doesn't cycle tools. That
    /// used to be positional -- everything below one early return in the
    /// dispatcher -- which silently assumed those actions would never be
    /// rebound to a chord.
    pub fn consume(&self, ctx: &egui::Context, action: Action) -> bool {
        let Some(shortcut) = self.shortcut(action) else {
            return false;
        };
        if Self::is_bare(&shortcut) && ctx.egui_wants_keyboard_input() {
            return false;
        }
        ctx.input_mut(|i| i.consume_shortcut(&shortcut))
    }

    fn is_bare(shortcut: &KeyboardShortcut) -> bool {
        let m = shortcut.modifiers;
        !m.alt && !m.ctrl && !m.command && !m.mac_cmd
    }

    /// Menu label with the chord in the shortcut column.
    pub fn menu_label(&self, ctx: &egui::Context, text: &str, action: Action) -> String {
        match self.shortcut(action) {
            Some(s) => format!("{text}\t{}", ctx.format_shortcut(&s)),
            None => text.to_owned(),
        }
    }

    /// A tooltip like "Highlight (H)", dropping the parenthetical when unbound.
    pub fn tooltip(&self, ctx: &egui::Context, action: Action) -> String {
        match self.shortcut(action) {
            Some(s) => format!("{} ({})", action.label(), ctx.format_shortcut(&s)),
            None => action.label().to_owned(),
        }
    }

    pub fn to_stored(&self) -> StoredKeymap {
        let mut entries: Vec<_> = self
            .overrides
            .iter()
            .map(|(a, s)| (*a, s.as_ref().map(StoredBinding::from_shortcut)))
            .collect();
        // Stable on disk so a no-op save doesn't churn the file.
        entries.sort_by_key(|(a, _)| format!("{a:?}"));
        StoredKeymap(entries)
    }

    pub fn from_stored(stored: StoredKeymap) -> Self {
        let mut map = Self::default();
        for (action, binding) in stored.0 {
            match binding {
                // A binding we can't parse (a key egui renamed, say) is
                // dropped rather than turned into "unbound", so the action
                // falls back to its default instead of going dead.
                Some(b) => {
                    if let Some(shortcut) = b.to_shortcut() {
                        map.overrides.insert(action, Some(shortcut));
                    }
                }
                None => {
                    map.overrides.insert(action, None);
                }
            }
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for action in Action::ALL {
            assert!(seen.insert(action), "{action:?} appears twice in ALL");
        }
        assert_eq!(seen.len(), Action::ALL.len());
    }

    #[test]
    fn every_action_has_a_label() {
        for action in Action::ALL {
            assert!(!action.label().is_empty(), "{action:?} has no label");
        }
    }

    #[test]
    fn defaults_do_not_collide_within_a_modifier_class() {
        let map = Keymap::default();
        for action in Action::ALL {
            let Some(shortcut) = map.shortcut(action) else {
                continue;
            };
            let clashes = map.conflicts(shortcut, action);
            assert!(
                clashes.is_empty(),
                "{action:?} shares its default with {clashes:?}"
            );
        }
    }

    #[test]
    fn every_default_survives_a_storage_round_trip() {
        for action in Action::ALL {
            let shortcut = action.default_shortcut().expect("a default");
            let stored = StoredBinding::from_shortcut(&shortcut);
            assert_eq!(
                stored.to_shortcut(),
                Some(shortcut),
                "{action:?} did not round-trip"
            );
        }
    }

    #[test]
    fn only_deviations_are_stored() {
        let mut map = Keymap::default();
        assert_eq!(map.to_stored().0.len(), 0);

        // Setting the value it already has is not a deviation.
        map.set(Action::Undo, Action::Undo.default_shortcut());
        assert_eq!(map.to_stored().0.len(), 0);

        map.set(
            Action::ToolHighlight,
            Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::J)),
        );
        assert_eq!(map.to_stored().0.len(), 1);
    }

    #[test]
    fn overrides_round_trip_through_storage() {
        let mut map = Keymap::default();
        let chord = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::H);
        map.set(Action::ToolHighlight, Some(chord));
        map.set(Action::Print, None); // deliberately unbound

        let restored = Keymap::from_stored(map.to_stored());
        assert_eq!(restored.shortcut(Action::ToolHighlight), Some(chord));
        assert_eq!(restored.shortcut(Action::Print), None);
        // Untouched actions still follow the defaults.
        assert_eq!(
            restored.shortcut(Action::Undo),
            Action::Undo.default_shortcut()
        );
    }

    #[test]
    fn resetting_restores_the_default() {
        let mut map = Keymap::default();
        map.set(
            Action::ToolPen,
            Some(KeyboardShortcut::new(Modifiers::NONE, Key::K)),
        );
        assert!(!map.is_default(Action::ToolPen));
        map.reset(Action::ToolPen);
        assert!(map.is_default(Action::ToolPen));
        assert_eq!(
            map.shortcut(Action::ToolPen),
            Action::ToolPen.default_shortcut()
        );
    }

    #[test]
    fn conflicts_finds_the_other_action_and_ignores_itself() {
        let mut map = Keymap::default();
        let undo = Action::Undo.default_shortcut().expect("a default");
        map.set(Action::ToolPen, Some(undo));

        assert_eq!(map.conflicts(undo, Action::ToolPen), vec![Action::Undo]);
        assert!(map.conflicts(undo, Action::Undo).contains(&Action::ToolPen));
    }

    #[test]
    fn an_unparsable_stored_key_falls_back_to_the_default() {
        let stored = StoredKeymap(vec![(
            Action::Undo,
            Some(StoredBinding {
                alt: false,
                ctrl: false,
                shift: false,
                mac_cmd: false,
                command: true,
                key: "NotAKeyName".to_owned(),
            }),
        )]);
        let map = Keymap::from_stored(stored);
        assert_eq!(map.shortcut(Action::Undo), Action::Undo.default_shortcut());
    }

    #[test]
    fn bare_chords_are_recognized() {
        let bare = KeyboardShortcut::new(Modifiers::NONE, Key::V);
        let shifted = KeyboardShortcut::new(Modifiers::SHIFT, Key::V);
        let chord = KeyboardShortcut::new(Modifiers::COMMAND, Key::V);
        assert!(Keymap::is_bare(&bare));
        // Shift alone still types characters, so it counts as bare.
        assert!(Keymap::is_bare(&shifted));
        assert!(!Keymap::is_bare(&chord));
    }
}
