//! `qui`: a small immediate-mode GUI toolkit for one window, on Wayland
//! (or X11).
//!
//! * [`font`]: TrueType parsing and rasterization (no font crates).
//! * [`text`]: glyph cache and atlas, measuring, eliding, wrapping.
//! * [`image`]: an RGBA atlas for icons, pictures and colour emoji.
//! * [`gfx`]: Vulkan through `ash`: one pipeline of instanced quads.
//! * [`input`]: input events.
//! * [`ui`]: widgets, layout helpers, input handling.
//! * [`window`]: the windowing: a normal window or a layer-shell overlay,
//!   and the event loop ([`run`]), on Wayland or, where there is no
//!   Wayland, X11 (the `wayland` and `x11` features, both on by default).
//!
//! It is event-driven: the UI is rebuilt after each input or app event,
//! and a frame is only rendered when the resulting quads differ from the
//! last frame. An idle window wakes up for nothing.

pub mod clipboard;
pub mod font;
pub mod gfx;
pub mod image;
pub mod input;
pub mod text;
pub mod ui;
pub mod window;

#[cfg(not(any(feature = "wayland", feature = "x11")))]
compile_error!("qui needs a windowing backend: the `wayland` or `x11` feature");

pub use clipboard::Pasted;
pub use input::{Cursor, Event, Key, KeyEvent, Mods, Named};
pub use ui::{Align, ButtonStyle, Color, Edit, EditStyle, Rect, Response, Theme, Ui, id};
pub use window::{App, Cx, Spec, Waker, run, system_theme};
