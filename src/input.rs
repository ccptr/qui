//! Input events as [`crate::Ui`] receives them from the windowing
//! ([`crate::window`]).

/// Keyboard modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Super / Command.
    pub logo: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { ctrl: false, shift: false, alt: false, logo: false };
}

/// Keys a widget reacts to by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Named {
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Escape,
    Tab,
    Insert,
    Paste,
    /// F1..F35.
    F(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Named(Named),
    /// A key that produces a character (the character with modifiers
    /// applied, e.g. `'A'` with Shift).
    Char(char),
    Other,
}

/// A key press (or repeat).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    /// Text the key produces, if any (none for most named keys).
    pub text: Option<String>,
    /// The XKB keysym, when the runner knows it (0 otherwise). Bindings
    /// written as key names ("Return", "Page_Down", "a") match on this.
    pub keysym: u32,
    /// The key's character without modifiers, for layouts with Latin
    /// letters (used to match Ctrl+V on any layout).
    pub base: Option<char>,
    /// The Linux evdev scan code (0 if unknown).
    pub code: u32,
    pub repeat: bool,
    /// Modifiers held when the key was pressed (they may have changed by
    /// the time the frame is built).
    pub mods: Mods,
}

/// evdev code of the V key (where V is on a US keyboard).
pub const CODE_V: u32 = 47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Pointer position in physical pixels.
    PointerMoved(f32, f32),
    PointerLeft,
    Button { button: Button, pressed: bool },
    /// Scroll by lines (wheel clicks), positive = content moves down.
    WheelLines(f32),
    /// Scroll by physical pixels (touchpads), positive = content moves down.
    WheelPixels(f32),
    Modifiers(Mods),
    Key(KeyEvent),
    /// Text committed by an input method.
    Commit(String),
    /// Keyboard focus entered or left the window.
    Focus(bool),
}

/// Pointer cursor shapes qui asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    #[default]
    Default,
    Pointer,
    Text,
}
