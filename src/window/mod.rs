//! qui's windowing: one surface at a time, either a layer-shell overlay
//! (launchers, panels) or a normal window, and the event loop ([`run`]).
//!
//! The app decides when its surface exists: [`Cx::show`] creates (or
//! updates) it and [`Cx::hide`] destroys it, freeing the swapchain; the
//! Vulkan device, the glyph and image atlases and the app's state live on,
//! so showing again is quick. A layer surface with
//! [`Spec::keep_when_hidden`] is instead *parked* when hidden: it keeps its
//! swapchain, so showing it again is immediate, and [`Cx::prepare`] creates
//! one parked up front.
//!
//! The loop sleeps until input, a [`Waker`] (usable from any thread) or a
//! key repeat; nothing else wakes it.
//!
//! # Backends
//!
//! [`run`] uses Wayland, as a native client (the `wayland` feature).
//! Without Wayland (no `WAYLAND_DISPLAY` but a `DISPLAY`) it runs the same
//! app on X11 instead (the `x11` feature). Both features are on by default,
//! and at least one is needed. `QUI_BACKEND=x11` or `wayland` picks one.
//!
//! On X11, as a client of libxcb (opened at run time, through x11rb), with
//! xkbcommon-x11 for the keyboard and Vulkan's `VK_KHR_xcb_surface`, this
//! differs from Wayland:
//!
//! * A layer-shell [`Spec`] becomes an override-redirect window (the window
//!   manager leaves it alone), placed as the layer surface would be (its
//!   anchors and margins) on the monitor under the pointer, and stacked
//!   above other windows ([`Layer::Top`], [`Layer::Overlay`]) or below
//!   them. The window is only the content: there is no catch-all area.
//!   With [`Keyboard::Exclusive`] it grabs the keyboard and the pointer
//!   while shown, so a click anywhere else is [`App::clicked_outside`];
//!   [`Keyboard::OnDemand`] gives it the focus when shown. Exclusive
//!   zones and blur are not supported.
//! * The scale is the resource database's `Xft.dpi` / 96, read when a
//!   window is created.
//! * Hiding a window with [`Spec::keep_when_hidden`] unmaps it (a toplevel
//!   too) and keeps its swapchain; [`Cx::prepare`] creates it unmapped,
//!   swapchain included.
//! * Key repeat comes from the server (detectable auto-repeat). There is
//!   no input method support.
//! * A transparent spec gets a 32-bit visual when a compositing manager
//!   runs; without one, it is drawn over the theme's background.
//! * A hidden window owns the clipboard, so copied text stays available
//!   while the app is hidden. Pastes and file drops (XDND) are read
//!   incrementally (INCR) when large.

use crate::clipboard::{IMAGE_EXTENSIONS, Pasted, image_mime, uri_paths};
use crate::gfx::{Batch, Gfx, Quad};
use crate::input::{Cursor, Event, Key, KeyEvent, Mods, Named};
use crate::ui::{Theme, Ui};
use std::ffi::{CString, c_char};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "wayland")]
use wayland_client::Connection;
use xkbcommon_dl as xkb;

#[cfg(feature = "wayland")]
mod wayland;
#[cfg(feature = "x11")]
mod x11;

// ---- public API -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Layer {
    Background,
    Bottom,
    Top,
    #[default]
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Keyboard {
    None,
    /// The surface keeps the keyboard while it exists.
    Exclusive,
    /// Focus like a normal window (usually given when shown).
    #[default]
    OnDemand,
}

/// A blurred area behind the content (`ext-background-effect-v1`), in
/// content coordinates; rounded corners are approximated with steps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blur {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
}

/// What surface to show and how.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// A layer-shell overlay, or (false, or without layer-shell) a toplevel.
    pub layer_shell: bool,
    pub layer: Layer,
    /// Anchored edges: top, right, bottom, left. Anchored on two opposite
    /// edges, the catch-all area spans the output on that axis; otherwise
    /// it is the content's size.
    pub anchor: [bool; 4],
    /// top, right, bottom, left.
    pub margin: [i32; 4],
    pub exclusive_zone: i32,
    pub keyboard: Keyboard,
    pub namespace: String,
    /// The content's size in logical pixels.
    pub width: u32,
    pub height: u32,
    /// The content is drawn over a transparent background.
    pub transparent: bool,
    pub blur: Option<Blur>,
    /// Toplevel title and app id.
    pub title: String,
    pub app_id: String,
    /// A toplevel the user can resize (the content size is then the
    /// initial size).
    pub resizable: bool,
    /// The smallest size of a resizable toplevel.
    pub min_size: (u32, u32),
    /// Park the surface when hidden instead of destroying it, so showing
    /// it again is immediate (layer surfaces only; costs the swapchain's
    /// memory while hidden). See the module docs.
    pub keep_when_hidden: bool,
}

impl Spec {
    /// A normal, resizable window.
    pub fn window(title: &str, app_id: &str, width: u32, height: u32) -> Spec {
        Spec {
            layer_shell: false,
            transparent: false,
            title: title.into(),
            app_id: app_id.into(),
            width,
            height,
            resizable: true,
            ..Spec::default()
        }
    }
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            layer_shell: true,
            layer: Layer::Overlay,
            anchor: [true; 4],
            margin: [0; 4],
            exclusive_zone: -1,
            keyboard: Keyboard::OnDemand,
            namespace: "qui".into(),
            width: 600,
            height: 400,
            transparent: true,
            blur: None,
            title: String::new(),
            app_id: "qui".into(),
            resizable: false,
            min_size: (0, 0),
            keep_when_hidden: false,
        }
    }
}

/// Requests from the app to the runner.
#[derive(Default)]
pub struct Cx {
    /// Create the surface, or update it (size, layer, ...) if it exists.
    pub show: Option<Spec>,
    pub hide: bool,
    /// With [`Cx::hide`]: also release the Vulkan device and the atlases'
    /// GPU copies, so a hidden app holds no GPU memory. Showing again then
    /// creates the device anew (tens of milliseconds).
    pub release_gpu: bool,
    /// Create the surface parked (hidden, its swapchain ready at the right
    /// scale), so a later [`Cx::show`] of the same kind is instant. Needs a
    /// layer-shell spec with [`Spec::keep_when_hidden`]; ignored while a
    /// surface exists.
    pub prepare: Option<Spec>,
    pub exit: bool,
    /// Build once more (state changed during the build).
    pub again: bool,
    /// Put this text on the clipboard.
    pub copy: Option<String>,
    /// Read everything the clipboard holds, for [`App::pasted`] (what
    /// Ctrl+V does outside a text field).
    pub paste: bool,
    pub(crate) visible: bool,
    pub(crate) scale: f32,
}

impl Cx {
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The surface's scale (parked ones too), or 1 without a surface.
    pub fn scale(&self) -> f32 {
        self.scale
    }
}

pub trait App: 'static {
    /// Called once before the loop starts (e.g. to show the surface).
    fn start(&mut self, _cx: &mut Cx) {}

    /// Builds the whole UI for this frame (only while shown).
    fn build(&mut self, ui: &mut Ui, cx: &mut Cx);

    /// The [`Waker`] was woken (coalesced). Return whether to rebuild.
    fn woken(&mut self, cx: &mut Cx) -> bool;

    /// The surface gained or lost keyboard focus.
    fn focus(&mut self, _focused: bool, _cx: &mut Cx) {}

    /// A click on the transparent area around the content.
    fn clicked_outside(&mut self, _cx: &mut Cx) {}

    /// The compositor closed the surface, or the user closed the window
    /// (it is gone already). Exits by default.
    fn closed(&mut self, cx: &mut Cx) {
        cx.exit = true;
    }

    /// Files were dropped onto the window.
    fn dropped(&mut self, _path: PathBuf) {}

    /// Ctrl+V outside a text field (or [`Cx::paste`]): everything the
    /// clipboard held, possibly nothing.
    fn pasted(&mut self, _content: Pasted) {}

    /// Reading the clipboard failed.
    fn clipboard_error(&mut self, _error: String) {}

    /// The theme the UI starts with: the desktop's light or dark preference.
    fn theme(&self) -> Theme {
        system_theme()
    }
}

/// [`Theme::LIGHT`] if the desktop prefers light (GNOME's `color-scheme`
/// setting, as the settings portal reports it), else [`Theme::DARK`].
pub fn system_theme() -> Theme {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) if String::from_utf8_lossy(&out.stdout).contains("prefer-light") => Theme::LIGHT,
        _ => Theme::DARK,
    }
}

/// Wakes the loop from any thread; [`App::woken`] runs on the UI thread.
#[derive(Clone)]
pub struct Waker(pub(crate) Arc<OwnedFd>);

impl Waker {
    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.0.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    pub(crate) fn new() -> Result<Waker, String> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err("eventfd failed".into());
        }
        Ok(Waker(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Clears a wake-up (after `poll` saw the fd readable).
    pub(crate) fn drain(&self) {
        let mut v = 0u64;
        unsafe { libc::read(self.0.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
    }
}

/// An XKB keysym by name ("Return", "Page_Down", "a"), as in GTK key
/// names; 0 if unknown or without libxkbcommon.
pub fn keysym_from_name(name: &str) -> u32 {
    let (Some(x), Ok(c)) = (xkb::xkbcommon_option(), CString::new(name)) else {
        return 0;
    };
    let sym = unsafe { (x.xkb_keysym_from_name)(c.as_ptr(), xkb::xkb_keysym_flags::XKB_KEYSYM_NO_FLAGS) };
    if sym != 0 {
        return sym;
    }
    unsafe { (x.xkb_keysym_from_name)(c.as_ptr(), xkb::xkb_keysym_flags::XKB_KEYSYM_CASE_INSENSITIVE) }
}

/// The lowercase keysym of a letter keysym (others unchanged).
pub fn keysym_to_lower(sym: u32) -> u32 {
    match sym {
        0x41..=0x5a => sym + 0x20,
        0xc0..=0xde if sym != 0xd7 => sym + 0x20,
        0x0100_0000.. => char::from_u32(sym - 0x0100_0000)
            .and_then(|c| c.to_lowercase().next())
            .map_or(sym, |c| 0x0100_0000 + c as u32),
        _ => sym,
    }
}

/// Runs until [`Cx::exit`]: on Wayland, or on X11 where there is no
/// Wayland (see the module docs).
pub fn run<A: App>(make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    match std::env::var("QUI_BACKEND").as_deref() {
        Ok("wayland") => return run_wayland(make),
        Ok("x11") => return run_x11(make),
        _ => {}
    }
    #[cfg(all(feature = "wayland", feature = "x11"))]
    {
        let env = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        if env("DISPLAY") {
            if !env("WAYLAND_DISPLAY") && !env("WAYLAND_SOCKET") {
                return x11::run(make);
            }
            return match Connection::connect_to_env() {
                Ok(conn) => wayland::run(conn, make),
                Err(e) => {
                    eprintln!("qui: cannot connect to Wayland ({e}); using X11");
                    x11::run(make)
                }
            };
        }
    }
    if cfg!(feature = "wayland") { run_wayland(make) } else { run_x11(make) }
}

#[cfg(feature = "wayland")]
fn run_wayland<A: App>(make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Cannot connect to Wayland: {e}"))?;
    wayland::run(conn, make)
}

#[cfg(not(feature = "wayland"))]
fn run_wayland<A: App>(_make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    Err("qui was built without Wayland support (the wayland feature)".into())
}

#[cfg(feature = "x11")]
use x11::run as run_x11;

#[cfg(not(feature = "x11"))]
fn run_x11<A: App>(_make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    Err("qui was built without X11 support (the x11 feature)".into())
}

// ---- shared by the backends ------------------------------------------------------

/// QUI_STATS: a line stamped with the monotonic clock, in ms (comparable
/// across processes, e.g. with the time a client sent a request).
pub(crate) fn stat(msg: std::fmt::Arguments) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("QUI_STATS").is_some() || std::env::var_os("RLS_STATS").is_some()) {
        return;
    }
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ms = ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6;
    eprintln!("qui: [{ms:.1}] {msg}");
}

/// What the backends share: the UI, the renderer and the last frame.
pub(crate) struct Painter {
    pub(crate) ui: Option<Ui>,
    pub(crate) gfx: Option<Gfx>,
    quads: Vec<Quad>,
    batches: Vec<Batch>,
    cursor: Cursor,
    /// A frame is built but not yet presented.
    pub(crate) dirty: bool,
    pub(crate) stats: bool,
    /// QUI_EVENTS: log every input event.
    trace: bool,
    started: std::time::Instant,
    frames: u64,
    /// When the surface was last created (for QUI_STATS).
    pub(crate) shown_at: Option<std::time::Instant>,
}

impl Painter {
    pub(crate) fn new() -> Painter {
        let env = |a, b| std::env::var_os(a).is_some() || std::env::var_os(b).is_some();
        Painter {
            ui: None,
            gfx: None,
            quads: Vec::new(),
            batches: Vec::new(),
            cursor: Cursor::Default,
            dirty: false,
            stats: env("QUI_STATS", "RLS_STATS"),
            trace: env("QUI_EVENTS", "RLS_EVENTS"),
            started: std::time::Instant::now(),
            frames: 0,
            shown_at: None,
        }
    }

    /// Feeds input to the UI. Returns whether to rebuild, and what a
    /// Ctrl+V asks for: `Some(true)` text for the focused field,
    /// `Some(false)` everything the clipboard holds.
    pub(crate) fn events(&mut self, events: &[Event]) -> (bool, Option<bool>) {
        if self.trace {
            for e in events {
                eprintln!("qui: {:>8} ms {e:?}", self.started.elapsed().as_millis());
            }
        }
        let Some(ui) = &mut self.ui else { return (false, None) };
        let mut rebuild = false;
        for e in events {
            rebuild |= ui.event(e);
        }
        let paste = std::mem::take(&mut ui.paste_key).then_some(ui.focus.is_some());
        (rebuild, paste)
    }

    /// Hands what a clipboard read or a drop brought to the UI or the app.
    pub(crate) fn fetched<A: App>(&mut self, app: &mut A, fetched: Fetched) {
        match fetched {
            Fetched::Field(text) => {
                if let Some(ui) = &mut self.ui {
                    ui.paste(&text);
                }
            }
            Fetched::Pasted(content) => app.pasted(content),
            Fetched::Dropped(paths) => {
                for path in paths {
                    app.dropped(path);
                }
            }
            Fetched::Error(e) => app.clipboard_error(e),
        }
    }

    pub(crate) fn ensure_ui<A: App>(&mut self, app: &A) -> Result<(), String> {
        if self.ui.is_none() {
            self.ui = Some(Ui::new(app.theme())?);
        }
        Ok(())
    }

    /// Builds the UI at a logical size; returns whether the output changed.
    pub(crate) fn build<A: App>(&mut self, app: &mut A, cx: &mut Cx, (w, h): (f32, f32), scale: f32, force: bool) -> bool {
        let Some(ui) = &mut self.ui else { return false };
        let mut changed = force;
        for _ in 0..4 {
            cx.again = false;
            ui.begin(w, h, scale);
            app.build(ui, cx);
            let Some((quads, batches)) = ui.finish() else { continue };
            if quads != self.quads.as_slice() || batches != self.batches.as_slice() {
                self.quads.clear();
                self.quads.extend_from_slice(quads);
                self.batches.clear();
                self.batches.extend_from_slice(batches);
                changed = true;
            }
            if !cx.again {
                break;
            }
        }
        changed
    }

    /// The cursor the UI wants, when it changed (or `resend` is set, which
    /// is cleared).
    pub(crate) fn cursor_change(&mut self, resend: &mut bool) -> Option<Cursor> {
        let ui = self.ui.as_ref()?;
        if ui.cursor != self.cursor || std::mem::take(resend) {
            self.cursor = ui.cursor;
            return Some(self.cursor);
        }
        None
    }

    /// Presents the built frame (over a transparent background, or the
    /// theme's).
    pub(crate) fn draw(&mut self, transparent: bool) -> Result<(), String> {
        let (Some(gfx), Some(ui)) = (&mut self.gfx, &mut self.ui) else { return Ok(()) };
        let clear = if transparent { [0.0; 4] } else { ui.theme.bg.floats() };
        if gfx.draw(&self.quads, &self.batches, clear, &mut ui.text, &mut ui.images)? {
            self.dirty = false;
            self.frames += 1;
            if self.stats {
                stat(format_args!("frame {} presented", self.frames));
                if let Some(at) = self.shown_at.take() {
                    eprintln!("qui: shown in {:.1} ms", at.elapsed().as_secs_f64() * 1000.0);
                }
            }
        }
        Ok(())
    }

    /// Forgets the last frame (the surface is gone), detaching the
    /// swapchain.
    pub(crate) fn detach(&mut self) {
        if let Some(gfx) = &mut self.gfx {
            gfx.detach();
        }
        self.quads.clear();
        self.batches.clear();
        self.dirty = false;
    }

    /// Drops the Vulkan device. The glyph atlas is uploaded whole to the
    /// next one; images are decoded again by the app as it draws them.
    pub(crate) fn release_gpu(&mut self) {
        self.gfx = None;
        if let Some(ui) = &mut self.ui {
            ui.text.dirty = Some([0, 0, crate::text::ATLAS, crate::text::ATLAS]);
            ui.images.clear();
        }
        // Return the driver's freed heap to the system.
        unsafe { libc::malloc_trim(0) };
    }
}

/// A keymap and its state (owned: dropping unrefs them).
pub(crate) struct Xkb {
    pub(crate) keymap: *mut xkb::xkb_keymap,
    pub(crate) state: *mut xkb::xkb_state,
}

impl Xkb {
    /// The event for a press of an XKB keycode (evdev + 8), and whether
    /// the key repeats.
    pub(crate) fn key(&self, code: u32) -> Option<(KeyEvent, bool)> {
        let x = xkb::xkbcommon_option()?;
        let sym = unsafe { (x.xkb_state_key_get_one_sym)(self.state, code) };
        let mut buf = [0 as c_char; 64];
        let n = unsafe { (x.xkb_state_key_get_utf8)(self.state, code, buf.as_mut_ptr(), buf.len()) };
        let text = (n > 0)
            .then(|| {
                let bytes: Vec<u8> = buf[..(n as usize).min(63)].iter().map(|&b| b as u8).collect();
                String::from_utf8(bytes).ok()
            })
            .flatten()
            .filter(|t| !t.chars().any(char::is_control));
        let base = {
            let mut syms: *const xkb::xkb_keysym_t = std::ptr::null();
            let n = unsafe { (x.xkb_keymap_key_get_syms_by_level)(self.keymap, code, 0, 0, &mut syms) };
            (n > 0)
                .then(|| char::from_u32(unsafe { (x.xkb_keysym_to_utf32)(*syms) }))
                .flatten()
                .filter(|c| *c != '\0')
        };
        let event = KeyEvent {
            key: named(sym).map(Key::Named).unwrap_or_else(|| {
                match char::from_u32(unsafe { (x.xkb_keysym_to_utf32)(sym) }) {
                    Some(c) if c != '\0' && !c.is_control() => Key::Char(c),
                    _ => Key::Other,
                }
            }),
            text,
            keysym: sym,
            base,
            code: code.saturating_sub(8),
            repeat: false,
            mods: self.mods(),
        };
        let repeats = unsafe { (x.xkb_keymap_key_repeats)(self.keymap, code) } != 0;
        Some((event, repeats))
    }

    pub(crate) fn mods(&self) -> Mods {
        let Some(x) = xkb::xkbcommon_option() else { return Mods::NONE };
        let active = |name: &[u8]| unsafe {
            (x.xkb_state_mod_name_is_active)(
                self.state,
                name.as_ptr().cast(),
                xkb::xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        Mods {
            ctrl: active(xkb::XKB_MOD_NAME_CTRL),
            shift: active(xkb::XKB_MOD_NAME_SHIFT),
            alt: active(xkb::XKB_MOD_NAME_ALT),
            logo: active(xkb::XKB_MOD_NAME_LOGO),
        }
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        if let Some(x) = xkb::xkbcommon_option() {
            unsafe {
                (x.xkb_state_unref)(self.state);
                (x.xkb_keymap_unref)(self.keymap);
            }
        }
    }
}

/// The result of reading a clipboard or drop offer.
pub(crate) enum Fetched {
    Field(String),
    Pasted(Pasted),
    Dropped(Vec<PathBuf>),
    /// Only Wayland reports failures to read.
    #[cfg_attr(not(feature = "wayland"), allow(dead_code))]
    Error(String),
}

/// Which forms of the clipboard's content to read for a paste.
#[derive(Clone, Copy)]
pub(crate) struct PastePlan {
    files: bool,
    /// The image format's extension.
    image: Option<&'static str>,
    text: Option<&'static str>,
}

impl PastePlan {
    pub(crate) fn new(mimes: &[String]) -> PastePlan {
        PastePlan {
            files: mimes.iter().any(|m| m == "text/uri-list"),
            image: IMAGE_EXTENSIONS.into_iter().find(|&ext| mimes.iter().any(|m| m == image_mime(ext))),
            text: text_mime(mimes),
        }
    }

    /// The MIME types to read, in order.
    pub(crate) fn mimes(self) -> impl Iterator<Item = &'static str> {
        [self.files.then_some("text/uri-list"), self.image.map(image_mime), self.text].into_iter().flatten()
    }

    /// The content, from the data read for [`PastePlan::mimes`] in order.
    pub(crate) fn assemble(self, data: Vec<Vec<u8>>) -> Pasted {
        let mut data = data.into_iter();
        let uris = if self.files { uri_list(&data.next().unwrap_or_default()) } else { Vec::new() };
        let image = self.image.and_then(|ext| {
            let bytes = data.next().unwrap_or_default();
            (!bytes.is_empty()).then_some((bytes, ext))
        });
        let text = self.text.and_then(|_| {
            let t = String::from_utf8_lossy(&data.next().unwrap_or_default()).into_owned();
            (!t.trim().is_empty()).then_some(t)
        });
        let mut pasted = Pasted { files: uri_paths(&uris), image, text };
        pasted.drop_repeats(&uris);
        pasted
    }
}

/// The URIs of a `text/uri-list`.
pub(crate) fn uri_list(data: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(data)
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The best plain-text MIME type offered.
pub(crate) fn text_mime(mimes: &[String]) -> Option<&'static str> {
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"]
        .into_iter()
        .find(|m| mimes.iter().any(|x| x == m))
}

/// The named key for a keysym.
fn named(sym: u32) -> Option<Named> {
    Some(match sym {
        0xff08 => Named::Backspace,
        0xff09 | 0xfe20 => Named::Tab,
        0xff0d | 0xff8d => Named::Enter,
        0xff1b => Named::Escape,
        0xffff | 0xff9f => Named::Delete,
        0xff50 | 0xff95 => Named::Home,
        0xff51 | 0xff96 => Named::Left,
        0xff52 | 0xff97 => Named::Up,
        0xff53 | 0xff98 => Named::Right,
        0xff54 | 0xff99 => Named::Down,
        0xff55 | 0xff9a => Named::PageUp,
        0xff56 | 0xff9b => Named::PageDown,
        0xff57 | 0xff9c => Named::End,
        0xff63 | 0xff9e => Named::Insert,
        0x1008ff6d => Named::Paste,
        0xffbe..=0xffe0 => Named::F((sym - 0xffbe + 1) as u8),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_keysyms() {
        assert_eq!(keysym_to_lower(0x41), 0x61);
        assert_eq!(keysym_to_lower(0x61), 0x61);
        assert_eq!(keysym_to_lower(0xff0d), 0xff0d);
        assert_eq!(keysym_to_lower(0x0100_0000 + 'Ж' as u32), 0x0100_0000 + 'ж' as u32);
    }
}
