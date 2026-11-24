//! `vkui`: a small immediate-mode GUI toolkit for one window.
//!
//! * [`font`]: TrueType parsing and rasterization (no font crates).
//! * [`text`]: glyph cache and atlas, measuring, eliding, wrapping.
//! * [`gfx`]: Vulkan through `ash`: one pipeline of instanced quads.
//! * [`image`]: an RGBA atlas for icons, pictures and colour emoji.
//! * [`input`]: input events, independent of the windowing library.
//! * [`ui`]: widgets, layout helpers, input handling.
//!
//! `run` drives an `App` in a normal window with winit (feature `winit`);
//! `wl` drives Wayland layer-shell surfaces natively (feature `wayland`).
//! Both are event-driven: the UI is rebuilt after each input or app event, and a frame is only rendered when
//! the resulting quads differ from the last frame. An idle window wakes up
//! for nothing.

pub mod font;
pub mod gfx;
pub mod image;
pub mod input;
pub mod text;
pub mod ui;
#[cfg(feature = "wayland")]
pub mod wl;
#[cfg(feature = "winit")]
mod winit_run;

pub use input::{Cursor, Event, Key, KeyEvent, Mods, Named};
pub use ui::{Align, ButtonStyle, Color, Edit, EditStyle, Rect, Response, Theme, Ui, id};
#[cfg(feature = "winit")]
pub use winit;
#[cfg(feature = "winit")]
pub use winit_run::{App, Cx, Pasted, run};

