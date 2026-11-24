//! vkui's windowing: a native Wayland client. It shows one surface at a
//! time, either a layer-shell overlay (launchers, panels) or a normal
//! window (an `xdg_toplevel`, with server-side decorations where the
//! compositor offers them).
//!
//! The app decides when its surface exists: [`Cx::show`] creates (or
//! updates) it and [`Cx::hide`] destroys it, freeing the swapchain; the
//! Vulkan device, the glyph and image atlases and the app's state live on,
//! so showing again is quick.
//!
//! A layer surface with [`Spec::keep_when_hidden`] is instead *parked* when
//! hidden: the catch-all surface loses its buffer and its layer role, so
//! nothing is mapped, but the content surface and its swapchain stay. To
//! show it again, the first frame is drawn into the parked content right
//! away and the layer role is given back; the compositor maps both with
//! its configure, a round trip later. [`Cx::prepare`] creates a parked
//! surface up front, so even the first show is instant. Parked surfaces
//! present with MAILBOX (a FIFO present may wait for a frame callback an
//! unmapped surface never gets); without it, hiding destroys as usual.
//!
//! A layer surface is two Wayland surfaces: a transparent one covering the
//! anchored area (a single-pixel buffer stretched by the viewporter, so it
//! costs no memory), which catches clicks outside the content
//! ([`App::clicked_outside`]), and the content as a subsurface centered in
//! it, which is the only thing rendered. A toplevel is one surface.
//!
//! The loop is one `poll` over the Wayland socket, a wake-up eventfd
//! ([`Waker`], usable from any thread) and the key-repeat timer; nothing
//! else wakes it.

use crate::clipboard::{IMAGE_EXTENSIONS, Pasted, image_mime, uri_paths};
use crate::gfx::{Batch, Gfx, Quad};
use crate::input::{Button, Cursor, Event, Key, KeyEvent, Mods, Named};
use crate::ui::{Theme, Ui};
use std::ffi::{CString, c_char, c_void};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_keyboard, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1, ext_background_effect_surface_v1,
};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon_dl as xkb;

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
            namespace: "vkui".into(),
            width: 600,
            height: 400,
            transparent: true,
            blur: None,
            title: String::new(),
            app_id: "vkui".into(),
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
    /// Create the surface parked (hidden, its swapchain ready), so a later
    /// [`Cx::show`] of the same kind is instant. Needs a layer-shell spec
    /// with [`Spec::keep_when_hidden`]; ignored while a surface exists.
    pub prepare: Option<Spec>,
    pub exit: bool,
    /// Build once more (state changed during the build).
    pub again: bool,
    /// Put this text on the clipboard.
    pub copy: Option<String>,
    /// Read everything the clipboard holds, for [`App::pasted`] (what
    /// Ctrl+V does outside a text field).
    pub paste: bool,
    visible: bool,
}

impl Cx {
    pub fn visible(&self) -> bool {
        self.visible
    }
}

pub trait App: 'static {
    /// Called once before the loop starts (e.g. to show the surface).
    fn start(&mut self, _cx: &mut Cx) {}

    /// Builds the whole UI for this frame: while shown, and once when a
    /// surface is prepared ([`Cx::visible`] is then false).
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
pub struct Waker(Arc<OwnedFd>);

impl Waker {
    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.0.as_raw_fd(), (&one as *const u64).cast(), 8) };
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

/// Runs until [`Cx::exit`].
pub fn run<A: App>(make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Cannot connect to Wayland: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("Wayland registry: {e}"))?;
    let qh = queue.handle();
    let mut state = State::new(&globals, &qh)?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;

    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake_fd < 0 {
        return Err("eventfd failed".into());
    }
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(wake_fd) });
    let timer_fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
    if timer_fd < 0 {
        return Err("timerfd failed".into());
    }
    state.timer = Some(unsafe { OwnedFd::from_raw_fd(timer_fd) });
    state.waker = Some(Waker(wake.clone()));

    let app = make(Waker(wake.clone()));
    let mut runner = Runner {
        app,
        conn: conn.clone(),
        qh: qh.clone(),
        ui: None,
        gfx: None,
        quads: Vec::new(),
        batches: Vec::new(),
        cursor: Cursor::Default,
        dirty: false,
        stats: std::env::var_os("VKUI_STATS").is_some() || std::env::var_os("RLS_STATS").is_some(),
        trace: std::env::var_os("VKUI_EVENTS").is_some() || std::env::var_os("RLS_EVENTS").is_some(),
        started: std::time::Instant::now(),
        frames: 0,
        shown_at: None,
    };
    let mut cx = Cx::default();
    runner.app.start(&mut cx);
    if runner.apply(&mut state, cx)? {
        return Ok(());
    }

    loop {
        queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        if runner.process(&mut state)? {
            return Ok(());
        }
        if state.frame_now {
            // A parked surface is being shown or prepared: draw now.
            continue;
        }
        conn.flush().map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else { continue };
        let wl_fd = guard.connection_fd().as_raw_fd();
        let timer = state.timer.as_ref().map_or(-1, |t| t.as_raw_fd());
        let mut fds = [
            libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: timer, events: libc::POLLIN, revents: 0 },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if n < 0 {
            drop(guard);
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("poll failed".into());
        }
        if fds[0].revents != 0 {
            if let Err(e) = guard.read() {
                return Err(format!("Wayland connection: {e}"));
            }
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            let mut v = 0u64;
            unsafe { libc::read(wake.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
            state.woken = true;
        }
        if fds[2].revents != 0 {
            let mut v = 0u64;
            unsafe { libc::read(timer, (&mut v as *mut u64).cast(), 8) };
            if let Some((_, key)) = &state.repeating {
                let mut key = key.clone();
                key.repeat = true;
                state.events.push(Event::Key(key));
            }
        }
    }
}

// ---- the runner -------------------------------------------------------------------

/// VKUI_STATS: a line stamped with the monotonic clock, in ms (comparable
/// across processes, e.g. with the time a client sent a request).
fn stat(msg: std::fmt::Arguments) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VKUI_STATS").is_some() || std::env::var_os("RLS_STATS").is_some()) {
        return;
    }
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ms = ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6;
    eprintln!("vkui: [{ms:.1}] {msg}");
}

struct Runner<A: App> {
    app: A,
    conn: Connection,
    qh: QueueHandle<State>,
    ui: Option<Ui>,
    gfx: Option<Gfx>,
    quads: Vec<Quad>,
    batches: Vec<Batch>,
    cursor: Cursor,
    /// A frame is built but not yet presented.
    dirty: bool,
    stats: bool,
    /// VKUI_EVENTS: log every input event.
    trace: bool,
    started: std::time::Instant,
    frames: u64,
    /// When the surface was last created (for VKUI_STATS).
    shown_at: Option<std::time::Instant>,
}

impl<A: App> Runner<A> {
    /// Handles what dispatching produced. Returns true to exit.
    fn process(&mut self, state: &mut State) -> Result<bool, String> {
        let mut rebuild = false;
        let mut force = std::mem::take(&mut state.frame_now);
        let mut cx = Cx { visible: state.visible(), ..Cx::default() };

        let fetched = std::mem::take(&mut *state.fetched.lock().unwrap());
        for f in fetched {
            rebuild = true;
            match f {
                Fetched::Field(text) => {
                    if let Some(ui) = &mut self.ui {
                        ui.paste(&text);
                    }
                }
                Fetched::Pasted(content) => self.app.pasted(content),
                Fetched::Dropped(paths) => {
                    state.finish_drop();
                    for path in paths {
                        self.app.dropped(path);
                    }
                }
                Fetched::Error(e) => self.app.clipboard_error(e),
            }
        }
        if std::mem::take(&mut state.woken) && self.app.woken(&mut cx) {
            rebuild = true;
        }
        if let Some(focused) = state.focus_changed.take() {
            self.app.focus(focused, &mut cx);
            rebuild = true;
        }
        if std::mem::take(&mut state.outside_click) {
            self.app.clicked_outside(&mut cx);
            rebuild = true;
        }
        if std::mem::take(&mut state.closed) {
            self.teardown(state);
            self.app.closed(&mut cx);
        }

        let events = std::mem::take(&mut state.events);
        if self.trace {
            for e in &events {
                eprintln!("vkui: {:>8} ms {e:?}", self.started.elapsed().as_millis());
            }
        }
        if let Some(ui) = &mut self.ui {
            for e in &events {
                rebuild |= ui.event(e);
            }
            if std::mem::take(&mut ui.paste_key) {
                if ui.focus.is_some() {
                    state.paste_field();
                } else {
                    state.paste_all();
                }
            }
        }
        // A new or resized surface: (re)attach the swapchain.
        if let Some(s) = &mut state.surface
            && s.needs_attach
            && s.size.is_some()
        {
            s.needs_attach = false;
            let (pw, ph) = s.physical();
            let display = self.conn.backend().display_ptr() as *mut c_void;
            let surface = s.content.id().as_ptr() as *mut c_void;
            match &mut self.gfx {
                Some(gfx) if s.fresh => unsafe { gfx.attach(display, surface, (pw, ph))? },
                Some(gfx) => gfx.resize(pw, ph),
                None => {
                    let gfx = unsafe { Gfx::wayland(display, surface, (pw, ph), s.spec.transparent) }?;
                    if self.stats {
                        eprintln!("vkui: rendering on {}", gfx.device_name);
                    }
                    self.gfx = Some(gfx);
                }
            }
            if let Some(gfx) = &mut self.gfx {
                gfx.set_mailbox(s.spec.keep_when_hidden && matches!(s.role, Role::Layer { .. }));
            }
            s.fresh = false;
            stat(format_args!("swapchain attached"));
            if self.ui.is_none() {
                self.ui = Some(Ui::new(self.app.theme())?);
            }
            force = true;
        }
        // A parked surface is drawn only when about to be shown (or
        // prepared), never for changes while hidden.
        let parked = state.surface.as_ref().is_some_and(|s| s.parked);
        if (force || (!parked && (rebuild || cx.again))) && self.build(state, &mut cx, force) {
            self.dirty = true;
        }
        if self.dirty && (force || !parked) {
            self.draw(state)?;
        }
        self.apply(state, cx)
    }

    /// Builds the UI; returns whether the output changed.
    fn build(&mut self, state: &mut State, cx: &mut Cx, force: bool) -> bool {
        let (Some(ui), Some(s)) = (&mut self.ui, &state.surface) else { return false };
        let Some((w, h)) = s.size else { return false };
        let scale = s.scale();
        let mut changed = force;
        for _ in 0..4 {
            cx.again = false;
            ui.begin(w as f32, h as f32, scale);
            self.app.build(ui, cx);
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
        if ui.cursor != self.cursor || std::mem::take(&mut state.cursor_dirty) {
            self.cursor = ui.cursor;
            state.set_cursor(self.cursor);
        }
        changed
    }

    fn draw(&mut self, state: &State) -> Result<(), String> {
        let (Some(gfx), Some(ui), Some(s)) = (&mut self.gfx, &mut self.ui, &state.surface) else {
            return Ok(());
        };
        if s.size.is_none() {
            return Ok(());
        }
        let clear = if s.spec.transparent { [0.0; 4] } else { ui.theme.bg.floats() };
        if gfx.draw(&self.quads, &self.batches, clear, &mut ui.text, &mut ui.images)? {
            self.dirty = false;
            self.frames += 1;
            if self.stats {
                stat(format_args!("frame {} presented", self.frames));
                if let Some(at) = self.shown_at.take() {
                    eprintln!("vkui: shown in {:.1} ms", at.elapsed().as_secs_f64() * 1000.0);
                }
            }
        }
        Ok(())
    }

    /// Applies the app's requests. Returns true to exit.
    fn apply(&mut self, state: &mut State, cx: Cx) -> Result<bool, String> {
        if let Some(text) = cx.copy {
            state.copy(text);
        }
        if cx.paste {
            state.paste_all();
        }
        if cx.exit {
            self.teardown(state);
            let _ = self.conn.flush();
            return Ok(true);
        }
        let mailbox = self.gfx.as_ref().is_some_and(Gfx::is_mailbox);
        if cx.hide && state.visible() {
            let park = !cx.release_gpu
                && mailbox
                && state
                    .surface
                    .as_ref()
                    .is_some_and(|s| s.spec.keep_when_hidden && matches!(s.role, Role::Layer { .. }));
            if park {
                state.park();
                self.dirty = false;
            } else {
                self.teardown(state);
            }
        }
        if cx.release_gpu && !state.visible() {
            self.teardown(state);
            self.release_gpu();
        }
        if let Some(spec) = cx.prepare
            && state.surface.is_none()
            && spec.keep_when_hidden
        {
            state.prepare(spec, &self.qh);
        }
        if let Some(spec) = cx.show {
            if !state.visible() {
                self.shown_at = Some(std::time::Instant::now());
                stat(format_args!("show"));
            }
            state.show(spec, &self.qh, mailbox)?;
        }
        Ok(false)
    }

    /// Drops the Vulkan device. The glyph atlas is uploaded whole to the
    /// next one; images are decoded again by the app as it draws them.
    fn release_gpu(&mut self) {
        self.gfx = None;
        if let Some(ui) = &mut self.ui {
            ui.text.dirty = Some([0, 0, crate::text::ATLAS, crate::text::ATLAS]);
            ui.images.clear();
        }
        // Return the driver's freed heap to the system.
        unsafe { libc::malloc_trim(0) };
    }

    fn teardown(&mut self, state: &mut State) {
        if let Some(gfx) = &mut self.gfx {
            gfx.detach();
        }
        state.destroy_surface();
        self.quads.clear();
        self.batches.clear();
        self.dirty = false;
    }
}

// ---- Wayland state ----------------------------------------------------------------

enum Role {
    Layer {
        /// `None` while parked.
        layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
        subsurface: wl_subsurface::WlSubsurface,
        /// The catch-all surface and its viewport.
        base: wl_surface::WlSurface,
        base_viewport: Option<wp_viewport::WpViewport>,
        buffer: Option<wl_buffer::WlBuffer>,
        /// `buffer` is attached to `base` (it is mapped, or will be).
        attached: bool,
        /// The catch-all area's configured size.
        area: Option<(u32, u32)>,
    },
    Toplevel {
        xdg: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
        decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
        /// The size of the last `xdg_toplevel.configure` (0: our choice).
        pending: (u32, u32),
    },
}

struct Surface {
    spec: Spec,
    role: Role,
    /// The rendered surface.
    content: wl_surface::WlSurface,
    viewport: Option<wp_viewport::WpViewport>,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    blur: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    /// Logical content size, once configured.
    size: Option<(u32, u32)>,
    /// Fractional scale × 120 (0: unknown).
    scale120: u32,
    /// Integer buffer scale, without fractional scaling.
    int_scale: i32,
    needs_attach: bool,
    /// The Vulkan surface must be created for `content` (new surface).
    fresh: bool,
    /// Hidden, kept for the next show (see the module docs).
    parked: bool,
}

impl Surface {
    fn scale(&self) -> f32 {
        if self.scale120 > 0 { self.scale120 as f32 / 120.0 } else { self.int_scale.max(1) as f32 }
    }
    fn physical(&self) -> (u32, u32) {
        let (w, h) = self.size.unwrap_or((1, 1));
        let s = self.scale();
        (((w as f32 * s).round() as u32).max(1), ((h as f32 * s).round() as u32).max(1))
    }
}

struct Xkb {
    keymap: *mut xkb::xkb_keymap,
    state: *mut xkb::xkb_state,
}

struct State {
    qh: QueueHandle<State>,
    compositor: wl_compositor::WlCompositor,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    single_pixel: Option<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1>,
    cursor_shape: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    blur_manager: Option<ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1>,
    decorations: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    /// Kept alive: input-method text arrives through it.
    _text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    /// Text an input method committed, delivered on its `done`.
    ime_commit: Option<String>,
    _seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    data_device: Option<wl_data_device::WlDataDevice>,

    surface: Option<Surface>,
    /// The last scale an output gave (`scale120`, `int_scale`): a new
    /// surface starts with it, so it rarely renders at the wrong scale.
    last_scale: (u32, i32),
    /// Build and draw before waiting for events (a parked surface is
    /// being shown or prepared).
    frame_now: bool,
    xkb_ctx: *mut xkb::xkb_context,
    xkb: Option<Xkb>,
    repeat: (i32, i32),
    repeating: Option<(u32, KeyEvent)>,
    timer: Option<OwnedFd>,
    waker: Option<Waker>,

    events: Vec<Event>,
    woken: bool,
    focus_changed: Option<bool>,
    outside_click: bool,
    closed: bool,
    /// Pointer is over the content (not the catch-all area).
    pointer_on_content: bool,
    pointer_serial: u32,
    /// The cursor must be set again (the pointer entered).
    cursor_dirty: bool,
    input_serial: u32,
    wheel_lines_this_frame: bool,

    offers: Vec<(wl_data_offer::WlDataOffer, Vec<String>)>,
    selection: Option<wl_data_offer::WlDataOffer>,
    source: Option<(wl_data_source::WlDataSource, Arc<str>)>,
    /// A drag over the window: its offer and whether it carries files.
    drag: Option<(wl_data_offer::WlDataOffer, bool)>,
    /// A dropped offer waiting for its data before `finish`.
    dropped: Option<wl_data_offer::WlDataOffer>,
    /// Clipboard and drop reads finished by threads.
    fetched: Arc<Mutex<Vec<Fetched>>>,
}

/// The result of reading a clipboard or drop offer.
enum Fetched {
    Field(String),
    Pasted(Pasted),
    Dropped(Vec<PathBuf>),
    Error(String),
}

impl State {
    fn new(globals: &GlobalList, qh: &QueueHandle<State>) -> Result<State, String> {
        let compositor = globals
            .bind::<wl_compositor::WlCompositor, _, _>(qh, 4..=6, ())
            .map_err(|_| "The compositor lacks wl_compositor v4")?;
        let xkb_ctx = match xkb::xkbcommon_option() {
            Some(x) => unsafe { (x.xkb_context_new)(xkb::xkb_context_flags::XKB_CONTEXT_NO_FLAGS) },
            None => std::ptr::null_mut(),
        };
        if xkb_ctx.is_null() {
            eprintln!("vkui: libxkbcommon is missing; keyboard input won't work");
        }
        let seat = globals.bind::<wl_seat::WlSeat, _, _>(qh, 1..=8, ()).ok();
        let data_manager = globals.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(qh, 1..=3, ()).ok();
        let text_input = match (
            globals.bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3, _, _>(qh, 1..=1, ()).ok(),
            &seat,
        ) {
            (Some(m), Some(s)) => Some(m.get_text_input(s, qh, ())),
            _ => None,
        };
        let data_device = match (&data_manager, &seat) {
            (Some(m), Some(s)) => Some(m.get_data_device(s, qh, ())),
            _ => None,
        };
        Ok(State {
            qh: qh.clone(),
            compositor,
            subcompositor: globals.bind(qh, 1..=1, ()).ok(),
            shm: globals.bind(qh, 1..=1, ()).ok(),
            layer_shell: globals.bind(qh, 1..=4, ()).ok(),
            wm_base: globals.bind(qh, 1..=5, ()).ok(),
            viewporter: globals.bind(qh, 1..=1, ()).ok(),
            fractional: globals.bind(qh, 1..=1, ()).ok(),
            single_pixel: globals.bind(qh, 1..=1, ()).ok(),
            cursor_shape: globals.bind(qh, 1..=1, ()).ok(),
            blur_manager: globals.bind(qh, 1..=1, ()).ok(),
            decorations: globals.bind(qh, 1..=1, ()).ok(),
            _text_input: text_input,
            ime_commit: None,
            data_manager,
            _seat: seat,
            keyboard: None,
            pointer: None,
            cursor_device: None,
            data_device,
            surface: None,
            last_scale: (0, 1),
            frame_now: false,
            xkb_ctx,
            xkb: None,
            repeat: (25, 600),
            repeating: None,
            timer: None,
            waker: None,
            events: Vec::new(),
            woken: false,
            focus_changed: None,
            outside_click: false,
            closed: false,
            pointer_on_content: false,
            pointer_serial: 0,
            cursor_dirty: false,
            input_serial: 0,
            wheel_lines_this_frame: false,
            offers: Vec::new(),
            selection: None,
            source: None,
            drag: None,
            dropped: None,
            fetched: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Whether a surface is shown (not parked).
    fn visible(&self) -> bool {
        self.surface.as_ref().is_some_and(|s| !s.parked)
    }

    /// Shows `spec`. A parked surface of the same kind is reused if
    /// `reuse_parked` (its swapchain presents without waiting).
    fn show(&mut self, spec: Spec, qh: &QueueHandle<State>, reuse_parked: bool) -> Result<(), String> {
        if let Some(s) = &self.surface
            && s.parked
        {
            let same = reuse_parked
                && spec.layer_shell
                && self.layer_shell.is_some()
                && s.spec.layer == spec.layer
                && matches!(s.role, Role::Layer { .. });
            if same {
                self.unpark(spec, qh);
                return Ok(());
            }
            self.destroy_surface();
        }
        if let Some(s) = &mut self.surface {
            if s.spec == spec {
                return Ok(());
            }
            let same_kind = s.spec.layer_shell == spec.layer_shell && s.spec.layer == spec.layer;
            if same_kind {
                let resized = (s.spec.width, s.spec.height) != (spec.width, spec.height);
                s.spec = spec;
                if let Role::Layer { layer: Some(layer), .. } = &s.role {
                    configure_layer(layer, &s.spec);
                }
                if resized {
                    s.size = Some((s.spec.width, s.spec.height));
                    if let Some(vp) = &s.viewport {
                        vp.set_destination(s.spec.width as i32, s.spec.height as i32);
                    }
                    s.needs_attach = true;
                    self.place_content();
                }
                self.update_blur();
                if let Some(Surface { role: Role::Layer { base, .. }, .. }) = &self.surface {
                    base.commit();
                }
                return Ok(());
            }
            self.destroy_surface();
        }
        self.create_surface(spec, qh, true)
    }

    /// Creates a parked surface with its swapchain (drawn once), if `spec`
    /// is a layer surface and the compositor has layer-shell.
    fn prepare(&mut self, spec: Spec, qh: &QueueHandle<State>) {
        if spec.layer_shell
            && self.layer_shell.is_some()
            && self.subcompositor.is_some()
            && self.viewporter.is_some()
            && self.create_surface(spec, qh, false).is_ok()
        {
            self.frame_now = true;
        }
    }

    /// Creates the surfaces; without `with_role`, a parked layer surface (sized
    /// by the spec, as there is no configure).
    fn create_surface(&mut self, spec: Spec, qh: &QueueHandle<State>, with_role: bool) -> Result<(), String> {
        let content = self.compositor.create_surface(qh, ());
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&content, qh, ()));
        let fractional = match (&self.fractional, &viewport) {
            (Some(f), Some(_)) => Some(f.get_fractional_scale(&content, qh, ())),
            _ => None,
        };
        let use_layer = spec.layer_shell && self.layer_shell.is_some() && self.subcompositor.is_some();
        if spec.layer_shell && !use_layer {
            eprintln!("vkui: the compositor has no layer-shell; showing a normal window");
        }
        let role = if use_layer {
            let base = self.compositor.create_surface(qh, ());
            let subsurface = self.subcompositor.as_ref().unwrap().get_subsurface(&content, &base, qh, ());
            subsurface.set_desync();
            let base_viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&base, qh, ()));
            let layer = with_role.then(|| {
                let layer = new_layer(self.layer_shell.as_ref().unwrap(), &base, &spec, qh);
                configure_layer(&layer, &spec);
                layer
            });
            base.commit();
            Role::Layer { layer, subsurface, base, base_viewport, buffer: None, attached: false, area: None }
        } else {
            let wm = self.wm_base.as_ref().ok_or("The compositor has neither layer-shell nor xdg-shell")?;
            let xdg = wm.get_xdg_surface(&content, qh, ());
            let toplevel = xdg.get_toplevel(qh, ());
            toplevel.set_title(spec.title.clone());
            toplevel.set_app_id(spec.app_id.clone());
            if spec.resizable {
                toplevel.set_min_size(spec.min_size.0 as i32, spec.min_size.1 as i32);
            } else {
                toplevel.set_min_size(spec.width as i32, spec.height as i32);
                toplevel.set_max_size(spec.width as i32, spec.height as i32);
            }
            let decoration = self.decorations.as_ref().map(|m| {
                let d = m.get_toplevel_decoration(&toplevel, qh, ());
                d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
                d
            });
            content.commit();
            Role::Toplevel { xdg, toplevel, decoration, pending: (0, 0) }
        };
        if let Some(vp) = &viewport {
            vp.set_destination(spec.width as i32, spec.height as i32);
        }
        let (scale120, int_scale) = self.last_scale;
        if viewport.is_none() && int_scale > 1 {
            content.set_buffer_scale(int_scale);
        }
        let parked = !with_role && use_layer;
        let size = (spec.width, spec.height);
        self.surface = Some(Surface {
            spec,
            role,
            content,
            viewport,
            fractional,
            blur: None,
            size: parked.then_some(size),
            scale120,
            int_scale,
            needs_attach: parked,
            fresh: true,
            parked,
        });
        self.update_blur();
        Ok(())
    }

    /// Gives a parked surface its layer role back and draws its first
    /// frame at once; the compositor maps it with the configure.
    fn unpark(&mut self, spec: Spec, qh: &QueueHandle<State>) {
        let (Some(s), Some(shell)) = (&mut self.surface, &self.layer_shell) else { return };
        if (s.spec.width, s.spec.height) != (spec.width, spec.height) {
            s.size = Some((spec.width, spec.height));
            if let Some(vp) = &s.viewport {
                vp.set_destination(spec.width as i32, spec.height as i32);
            }
            s.needs_attach = true;
        }
        s.spec = spec;
        s.parked = false;
        let Role::Layer { layer, base, .. } = &mut s.role else { return };
        let l = new_layer(shell, base, &s.spec, qh);
        configure_layer(&l, &s.spec);
        base.commit();
        *layer = Some(l);
        self.update_blur();
        self.frame_now = true;
    }

    /// Hides a layer surface but keeps it: the catch-all surface drops its
    /// buffer (unmapping the content too) and its layer role.
    fn park(&mut self) {
        self.repeating = None;
        self.disarm_repeat();
        self.pointer_on_content = false;
        let Some(s) = &mut self.surface else { return };
        let Role::Layer { layer, base, attached, area, .. } = &mut s.role else { return };
        base.attach(None, 0, 0);
        base.commit();
        if let Some(l) = layer.take() {
            l.destroy();
        }
        *attached = false;
        *area = None;
        s.parked = true;
    }

    /// Centers the content in the catch-all area.
    fn place_content(&mut self) {
        let Some(s) = &self.surface else { return };
        if let Role::Layer { subsurface, area: Some((aw, ah)), base, .. } = &s.role {
            let x = (*aw as i32 - s.spec.width as i32) / 2;
            let y = (*ah as i32 - s.spec.height as i32) / 2;
            subsurface.set_position(x.max(0), y.max(0));
            base.commit();
        }
    }

    /// Sets the blur region; it applies with the content's next commit.
    fn update_blur(&mut self) {
        let (Some(m), Some(s)) = (&self.blur_manager, &mut self.surface) else { return };
        let Some(b) = s.spec.blur else {
            if let Some(e) = s.blur.take() {
                e.destroy();
            }
            return;
        };
        let effect = s.blur.get_or_insert_with(|| m.get_background_effect(&s.content, &self.qh, ()));
        let region = self.compositor.create_region(&self.qh, ());
        add_rounded(&region, b);
        effect.set_blur_region(Some(&region));
        region.destroy();
    }

    fn destroy_surface(&mut self) {
        self.repeating = None;
        self.disarm_repeat();
        let Some(s) = self.surface.take() else { return };
        if let Some(b) = s.blur {
            b.destroy();
        }
        if let Some(f) = s.fractional {
            f.destroy();
        }
        if let Some(v) = s.viewport {
            v.destroy();
        }
        match s.role {
            Role::Layer { layer, subsurface, base, base_viewport, buffer, .. } => {
                subsurface.destroy();
                s.content.destroy();
                if let Some(l) = layer {
                    l.destroy();
                }
                if let Some(v) = base_viewport {
                    v.destroy();
                }
                base.destroy();
                if let Some(b) = buffer {
                    b.destroy();
                }
            }
            Role::Toplevel { xdg, toplevel, decoration, .. } => {
                if let Some(d) = decoration {
                    d.destroy();
                }
                toplevel.destroy();
                xdg.destroy();
                s.content.destroy();
            }
        }
        self.pointer_on_content = false;
    }

    fn set_cursor(&self, cursor: Cursor) {
        if let Some(d) = &self.cursor_device {
            use wp_cursor_shape_device_v1::Shape;
            d.set_shape(
                self.pointer_serial,
                match cursor {
                    Cursor::Default => Shape::Default,
                    Cursor::Pointer => Shape::Pointer,
                    Cursor::Text => Shape::Text,
                },
            );
        }
    }

    fn disarm_repeat(&self) {
        if let Some(t) = &self.timer {
            let spec: libc::itimerspec = unsafe { std::mem::zeroed() };
            unsafe { libc::timerfd_settime(t.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
        }
    }

    fn arm_repeat(&self) {
        let (rate, delay) = self.repeat;
        let Some(t) = &self.timer else { return };
        if rate <= 0 {
            return;
        }
        let ms = |v: i64| libc::timespec { tv_sec: v / 1000, tv_nsec: (v % 1000) * 1_000_000 };
        let spec = libc::itimerspec { it_value: ms(delay.max(1) as i64), it_interval: ms((1000 / rate as i64).max(1)) };
        unsafe { libc::timerfd_settime(t.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
    }

    /// The selection's MIME types.
    fn selection_types(&self) -> Option<(&wl_data_offer::WlDataOffer, &[String])> {
        let offer = self.selection.as_ref()?;
        let (_, mimes) = self.offers.iter().find(|(o, _)| o == offer)?;
        Some((offer, mimes))
    }

    /// Reads the clipboard's text for the focused text field.
    fn paste_field(&mut self) {
        let Some((offer, mimes)) = self.selection_types() else { return };
        let Some(mime) = text_mime(mimes) else { return };
        let Some(read) = receive(offer, mime) else { return };
        self.fetch(vec![read], |mut data| {
            let text = String::from_utf8_lossy(&data.remove(0)).into_owned();
            Fetched::Field(text)
        });
    }

    /// Reads every form the clipboard holds (files, an image, text).
    fn paste_all(&mut self) {
        let Some((offer, mimes)) = self.selection_types() else {
            self.fetched.lock().unwrap().push(Fetched::Pasted(Pasted::default()));
            self.woken = true;
            return;
        };
        let files = mimes.iter().any(|m| m == "text/uri-list");
        let image = IMAGE_EXTENSIONS.into_iter().find(|&ext| mimes.iter().any(|m| m == image_mime(ext)));
        let text = text_mime(mimes);
        let mut reads = Vec::new();
        for mime in [files.then_some("text/uri-list"), image.map(image_mime), text].into_iter().flatten() {
            match receive(offer, mime) {
                Some(fd) => reads.push(fd),
                None => {
                    self.fetched.lock().unwrap().push(Fetched::Error("Cannot read the clipboard".into()));
                    self.woken = true;
                    return;
                }
            }
        }
        self.fetch(reads, move |data| {
            let mut data = data.into_iter();
            let uris: Vec<String> = if files {
                String::from_utf8_lossy(&data.next().unwrap_or_default())
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            } else {
                Vec::new()
            };
            let image = image.and_then(|ext| {
                let bytes = data.next().unwrap_or_default();
                (!bytes.is_empty()).then_some((bytes, ext))
            });
            let text = text.and_then(|_| {
                let t = String::from_utf8_lossy(&data.next().unwrap_or_default()).into_owned();
                (!t.trim().is_empty()).then_some(t)
            });
            let mut pasted = Pasted { files: uri_paths(&uris), image, text };
            pasted.drop_repeats(&uris);
            Fetched::Pasted(pasted)
        });
    }

    /// Reads the pipes in a thread, then hands `done` their contents.
    fn fetch(&self, reads: Vec<OwnedFd>, done: impl FnOnce(Vec<Vec<u8>>) -> Fetched + Send + 'static) {
        let out = self.fetched.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let data = reads
                .into_iter()
                .map(|fd| {
                    let mut buf = Vec::new();
                    let _ = std::fs::File::from(fd).take(64 << 20).read_to_end(&mut buf);
                    buf
                })
                .collect();
            out.lock().unwrap().push(done(data));
            if let Some(w) = waker {
                w.wake();
            }
        });
    }

    fn finish_drop(&mut self) {
        if let Some(offer) = self.dropped.take() {
            if offer.version() >= 3 {
                offer.finish();
            }
            offer.destroy();
        }
    }

    fn copy(&mut self, text: String) {
        let (Some(m), Some(dd)) = (&self.data_manager, &self.data_device) else { return };
        let source = m.create_data_source(&self.qh, ());
        for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"] {
            source.offer(mime.to_string());
        }
        dd.set_selection(Some(&source), self.input_serial);
        self.source = Some((source, text.into()));
    }

    fn handle_key(&mut self, key: u32, pressed: bool) {
        let code = key + 8;
        if !pressed {
            if self.repeating.as_ref().is_some_and(|(c, _)| *c == code) {
                self.repeating = None;
                self.disarm_repeat();
            }
            return;
        }
        let (Some(x), Some(k)) = (xkb::xkbcommon_option(), &self.xkb) else { return };
        let sym = unsafe { (x.xkb_state_key_get_one_sym)(k.state, code) };
        let mut buf = [0 as c_char; 64];
        let n = unsafe { (x.xkb_state_key_get_utf8)(k.state, code, buf.as_mut_ptr(), buf.len()) };
        let text = (n > 0)
            .then(|| {
                let bytes: Vec<u8> = buf[..(n as usize).min(63)].iter().map(|&b| b as u8).collect();
                String::from_utf8(bytes).ok()
            })
            .flatten()
            .filter(|t| !t.chars().any(char::is_control));
        let base = {
            let mut syms: *const xkb::xkb_keysym_t = std::ptr::null();
            let n = unsafe { (x.xkb_keymap_key_get_syms_by_level)(k.keymap, code, 0, 0, &mut syms) };
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
            code: key,
            repeat: false,
            mods: self.mods(),
        };
        let repeats = unsafe { (x.xkb_keymap_key_repeats)(k.keymap, code) } != 0;
        if repeats {
            self.repeating = Some((code, event.clone()));
            self.arm_repeat();
        } else {
            self.repeating = None;
            self.disarm_repeat();
        }
        self.events.push(Event::Key(event));
    }

    fn mods(&self) -> Mods {
        let (Some(x), Some(k)) = (xkb::xkbcommon_option(), &self.xkb) else { return Mods::NONE };
        let active = |name: &[u8]| unsafe {
            (x.xkb_state_mod_name_is_active)(
                k.state,
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

fn layer_size(spec: &Spec) -> (u32, u32) {
    let [top, right, bottom, left] = spec.anchor;
    // Stretched (0) on an axis anchored at both ends.
    (if left && right { 0 } else { spec.width }, if top && bottom { 0 } else { spec.height })
}

fn new_layer(
    shell: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
    base: &wl_surface::WlSurface,
    spec: &Spec,
    qh: &QueueHandle<State>,
) -> zwlr_layer_surface_v1::ZwlrLayerSurfaceV1 {
    let layer = match spec.layer {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    };
    shell.get_layer_surface(base, None, layer, spec.namespace.clone(), qh, ())
}

fn configure_layer(layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, spec: &Spec) {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let [top, right, bottom, left] = spec.anchor;
    let mut anchor = Anchor::empty();
    for (on, edge) in [(top, Anchor::Top), (right, Anchor::Right), (bottom, Anchor::Bottom), (left, Anchor::Left)] {
        if on {
            anchor |= edge;
        }
    }
    let (w, h) = layer_size(spec);
    layer.set_size(w, h);
    layer.set_anchor(anchor);
    layer.set_exclusive_zone(spec.exclusive_zone);
    let [mt, mr, mb, ml] = spec.margin;
    layer.set_margin(mt, mr, mb, ml);
    let interactivity = match spec.keyboard {
        Keyboard::None => KeyboardInteractivity::None,
        Keyboard::Exclusive => KeyboardInteractivity::Exclusive,
        // OnDemand needs v4; older ones only know none and exclusive.
        Keyboard::OnDemand if layer.version() >= 4 => KeyboardInteractivity::OnDemand,
        Keyboard::OnDemand => KeyboardInteractivity::Exclusive,
    };
    layer.set_keyboard_interactivity(interactivity);
}

/// A rounded rectangle as a region: the middle band plus a stack of thin
/// rows for each corner.
fn add_rounded(region: &wl_region::WlRegion, b: Blur) {
    let (x, y, w, h) = (b.x.round() as i32, b.y.round() as i32, b.w.round() as i32, b.h.round() as i32);
    let r = (b.radius.round() as i32).clamp(0, w.min(h) / 2);
    region.add(x, y + r, w, h - 2 * r);
    for i in 0..r {
        // Inset of the row i pixels from the top (and bottom) edge.
        let dy = (r - i) as f32 - 0.5;
        let inset = r - ((r * r) as f32 - dy * dy).max(0.0).sqrt().round() as i32;
        region.add(x + inset, y + i, w - 2 * inset, 1);
        region.add(x + inset, y + h - 1 - i, w - 2 * inset, 1);
    }
}

/// The best plain-text MIME type offered.
fn text_mime(mimes: &[String]) -> Option<&'static str> {
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"]
        .into_iter()
        .find(|m| mimes.iter().any(|x| x == m))
}

/// Asks the offer's owner to write `mime` into a pipe; returns its read end.
fn receive(offer: &wl_data_offer::WlDataOffer, mime: &str) -> Option<OwnedFd> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    offer.receive(mime.to_string(), write.as_fd());
    Some(read)
}

/// A 1×1 fully transparent buffer.
fn transparent_buffer(state: &State, qh: &QueueHandle<State>) -> Option<wl_buffer::WlBuffer> {
    if let Some(sp) = &state.single_pixel {
        return Some(sp.create_u32_rgba_buffer(0, 0, 0, 0, qh, ()));
    }
    let shm = state.shm.as_ref()?;
    let fd = unsafe { libc::memfd_create(c"vkui".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), 4) } != 0 {
        return None;
    }
    let pool = shm.create_pool(fd.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Some(buffer)
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

// ---- protocol events ----------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_subcompositor::WlSubcompositor);
delegate_noop!(State: wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wp_viewporter::WpViewporter);
delegate_noop!(State: wp_viewport::WpViewport);
delegate_noop!(State: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(State: wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(State: wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(State: zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(State: ignore ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1);
delegate_noop!(State: ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1);
delegate_noop!(State: zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
delegate_noop!(State: zwp_text_input_manager_v3::ZwpTextInputManagerV3);

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for State {
    fn event(state: &mut Self, ti: &zwp_text_input_v3::ZwpTextInputV3, event: zwp_text_input_v3::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwp_text_input_v3::{ContentHint, ContentPurpose, Event as E};
        match event {
            E::Enter { .. } => {
                ti.enable();
                ti.set_content_type(ContentHint::None, ContentPurpose::Normal);
                ti.commit();
            }
            E::Leave { .. } => {
                ti.disable();
                ti.commit();
            }
            E::CommitString { text } => state.ime_commit = text,
            E::Done { .. } => {
                if let Some(text) = state.ime_commit.take()
                    && !text.is_empty()
                {
                    state.events.push(Event::Commit(text));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(state: &mut Self, surface: &wl_surface::WlSurface, event: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let wl_surface::Event::PreferredBufferScale { factor } = event else { return };
        let Some(s) = &mut state.surface else { return };
        if *surface != s.content || s.fractional.is_some() || s.int_scale == factor {
            return;
        }
        let before = s.scale();
        s.int_scale = factor;
        state.last_scale = (s.scale120, s.int_scale);
        if s.viewport.is_none() {
            s.content.set_buffer_scale(factor);
        }
        if s.size.is_some() && s.scale() != before {
            s.needs_attach = true;
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for State {
    fn event(state: &mut Self, _: &wp_fractional_scale_v1::WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else { return };
        let Some(s) = &mut state.surface else { return };
        if s.scale120 != scale {
            let before = s.scale();
            s.scale120 = scale;
            state.last_scale = (s.scale120, s.int_scale);
            if s.size.is_some() && s.scale() != before {
                s.needs_attach = true;
            }
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(state: &mut Self, layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                stat(format_args!("layer configured ({width}x{height})"));
                layer.ack_configure(serial);
                let buffer = match &state.surface {
                    Some(Surface { role: Role::Layer { buffer: None, .. }, .. }) => transparent_buffer(state, qh),
                    _ => None,
                };
                let Some(s) = &mut state.surface else { return };
                let spec_size = (s.spec.width, s.spec.height);
                let Role::Layer { base, base_viewport, buffer: slot, attached, area, .. } = &mut s.role else { return };
                let aw = if width == 0 { spec_size.0 } else { width };
                let ah = if height == 0 { spec_size.1 } else { height };
                *area = Some((aw, ah));
                if let Some(b) = buffer {
                    *slot = Some(b);
                }
                if !*attached && let Some(b) = slot {
                    base.attach(Some(b), 0, 0);
                    *attached = true;
                }
                if let Some(vp) = base_viewport {
                    vp.set_destination(aw as i32, ah as i32);
                }
                base.damage_buffer(0, 0, i32::MAX, i32::MAX);
                if s.size.is_none() {
                    s.size = Some(spec_size);
                    s.needs_attach = true;
                }
                state.place_content();
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(state: &mut Self, xdg: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            let Some(s) = &mut state.surface else { return };
            let Role::Toplevel { pending, .. } = &s.role else { return };
            let size = match *pending {
                (w, h) if w > 0 && h > 0 && s.spec.resizable => {
                    (w.max(s.spec.min_size.0.max(1)), h.max(s.spec.min_size.1.max(1)))
                }
                _ => s.size.unwrap_or((s.spec.width, s.spec.height)),
            };
            if s.size != Some(size) {
                s.size = Some(size);
                if let Some(vp) = &s.viewport {
                    vp.set_destination(size.0 as i32, size.1 as i32);
                }
                s.needs_attach = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Close => state.closed = true,
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(Surface { role: Role::Toplevel { pending, .. }, .. }) = &mut state.surface {
                    *pending = (width.max(0) as u32, height.max(0) as u32);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event else { return };
        if caps.contains(wl_seat::Capability::Keyboard) {
            if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        } else if let Some(k) = state.keyboard.take() {
            // The last keyboard went away: a new one is a new object.
            if k.version() >= 3 {
                k.release();
            }
            state.repeating = None;
            state.disarm_repeat();
        }
        if caps.contains(wl_seat::Capability::Pointer) {
            if state.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                state.cursor_device = state.cursor_shape.as_ref().map(|m| m.get_pointer(&pointer, qh, ()));
                state.pointer = Some(pointer);
            }
        } else if let Some(p) = state.pointer.take() {
            if let Some(d) = state.cursor_device.take() {
                d.destroy();
            }
            if p.version() >= 3 {
                p.release();
            }
            state.pointer_on_content = false;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                let Some(x) = xkb::xkbcommon_option() else { return };
                if state.xkb_ctx.is_null() {
                    return;
                }
                let map = unsafe {
                    libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0)
                };
                if map == libc::MAP_FAILED {
                    return;
                }
                let keymap = unsafe {
                    (x.xkb_keymap_new_from_buffer)(
                        state.xkb_ctx,
                        map.cast(),
                        (size as usize).saturating_sub(1),
                        xkb::xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                        xkb::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                unsafe { libc::munmap(map, size as usize) };
                if keymap.is_null() {
                    return;
                }
                let xstate = unsafe { (x.xkb_state_new)(keymap) };
                if let Some(old) = state.xkb.take() {
                    unsafe {
                        (x.xkb_state_unref)(old.state);
                        (x.xkb_keymap_unref)(old.keymap);
                    }
                }
                state.xkb = Some(Xkb { keymap, state: xstate });
            }
            wl_keyboard::Event::Enter { serial, .. } => {
                state.input_serial = serial;
                state.focus_changed = Some(true);
                state.events.push(Event::Focus(true));
            }
            wl_keyboard::Event::Leave { .. } => {
                state.repeating = None;
                state.disarm_repeat();
                state.focus_changed = Some(false);
                state.events.push(Event::Focus(false));
                state.events.push(Event::Modifiers(Mods::NONE));
            }
            wl_keyboard::Event::Key { serial, key, state: key_state, .. } => {
                state.input_serial = serial;
                let pressed = key_state == WEnum::Value(wl_keyboard::KeyState::Pressed);
                state.handle_key(key, pressed);
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let (Some(x), Some(k)) = (xkb::xkbcommon_option(), &state.xkb) {
                    unsafe { (x.xkb_state_update_mask)(k.state, mods_depressed, mods_latched, mods_locked, 0, 0, group) };
                }
                let mods = state.mods();
                state.events.push(Event::Modifiers(mods));
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => state.repeat = (rate, delay),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let scale = state.surface.as_ref().map_or(1.0, Surface::scale);
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                state.pointer_serial = serial;
                state.cursor_dirty = true;
                state.pointer_on_content = state.surface.as_ref().is_some_and(|s| s.content == surface);
                if state.pointer_on_content {
                    state.events.push(Event::PointerMoved(surface_x as f32 * scale, surface_y as f32 * scale));
                } else {
                    state.events.push(Event::PointerLeft);
                }
                // The cursor is set on the next build (the runner resends it).
                state.woken = true;
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_on_content = false;
                state.events.push(Event::PointerLeft);
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } if state.pointer_on_content => {
                state.events.push(Event::PointerMoved(surface_x as f32 * scale, surface_y as f32 * scale));
            }
            wl_pointer::Event::Button { serial, button, state: bs, .. } => {
                state.input_serial = serial;
                let pressed = bs == WEnum::Value(wl_pointer::ButtonState::Pressed);
                if state.pointer_on_content {
                    let button = match button {
                        0x110 => Button::Left,
                        0x111 => Button::Right,
                        0x112 => Button::Middle,
                        _ => Button::Other,
                    };
                    state.events.push(Event::Button { button, pressed });
                } else if pressed {
                    state.outside_click = true;
                }
            }
            wl_pointer::Event::AxisValue120 { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value120 }
                if state.pointer_on_content =>
            {
                state.wheel_lines_this_frame = true;
                state.events.push(Event::WheelLines(-value120 as f32 / 120.0));
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. }
                if state.pointer_on_content && !state.wheel_lines_this_frame =>
            {
                state.events.push(Event::WheelPixels(-value as f32 * scale));
            }
            wl_pointer::Event::Frame => state.wheel_lines_this_frame = false,
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &wl_data_device::WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::DataOffer { id } => state.offers.push((id, Vec::new())),
            wl_data_device::Event::Selection { id } => {
                // Offers other than the selection and a drag are done.
                let drag = state.drag.as_ref().map(|d| d.0.clone());
                let dropped = state.dropped.clone();
                state.offers.retain(|(o, _)| {
                    let keep = id.as_ref() == Some(o) || drag.as_ref() == Some(o) || dropped.as_ref() == Some(o);
                    if !keep {
                        o.destroy();
                    }
                    keep
                });
                state.selection = id;
            }
            wl_data_device::Event::Enter { serial, id: Some(offer), .. } => {
                let files = state.offers.iter().any(|(o, m)| *o == offer && m.iter().any(|m| m == "text/uri-list"));
                if files {
                    offer.accept(serial, Some("text/uri-list".into()));
                    if offer.version() >= 3 {
                        use wl_data_device_manager::DndAction;
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                } else {
                    offer.accept(serial, None);
                }
                state.drag = Some((offer, files));
            }
            wl_data_device::Event::Leave => {
                if let Some((offer, _)) = state.drag.take() {
                    state.offers.retain(|(o, _)| *o != offer);
                    offer.destroy();
                }
            }
            wl_data_device::Event::Drop => {
                let Some((offer, files)) = state.drag.take() else { return };
                state.offers.retain(|(o, _)| *o != offer);
                if !files {
                    offer.destroy();
                    return;
                }
                if let Some(read) = receive(&offer, "text/uri-list") {
                    state.fetch(vec![read], |mut data| {
                        let uris: Vec<String> = String::from_utf8_lossy(&data.remove(0))
                            .lines()
                            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
                            .map(str::to_string)
                            .collect();
                        Fetched::Dropped(uri_paths(&uris))
                    });
                }
                state.dropped = Some(offer);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(state: &mut Self, offer: &wl_data_offer::WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some((_, mimes)) = state.offers.iter_mut().find(|(o, _)| o == offer)
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(state: &mut Self, source: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_source::Event::Send { fd, .. } => {
                if let Some((s, text)) = &state.source
                    && s == source
                {
                    let text = text.clone();
                    std::thread::spawn(move || {
                        use std::io::Write;
                        let _ = std::fs::File::from(fd).write_all(text.as_bytes());
                    });
                }
            }
            wl_data_source::Event::Cancelled => {
                source.destroy();
                if state.source.as_ref().is_some_and(|(s, _)| s == source) {
                    state.source = None;
                }
            }
            _ => {}
        }
    }
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

    #[test]
    fn layer_size_stretches_doubly_anchored_axes() {
        let spec = Spec { anchor: [true, false, true, false], width: 600, height: 500, ..Spec::default() };
        assert_eq!(layer_size(&spec), (600, 0));
    }
}
