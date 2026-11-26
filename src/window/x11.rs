//! The X11 backend, for desktops without Wayland: [`super::run`] runs the
//! app here when there is a `DISPLAY` but no Wayland. See the parent
//! module for what differs from Wayland.

use crate::gfx::{Gfx, Native};
use crate::input::{Button, Cursor, Event, Mods};
use super::{App, Cx, Fetched, Keyboard, Layer, Painter, PastePlan, Spec, Waker, Xkb, place, stat, text_mime, uri_list};
use crate::clipboard::{IMAGE_EXTENSIONS, image_mime, uri_paths};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::properties::{WmHints, WmSizeHints};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xkb::{self as xxkb, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _,
    CreateWindowAux, EventMask, GrabMode, GrabStatus, InputFocus, NotifyMode, PropMode, StackMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};
use xkbcommon_dl as xkb;
use xkbcommon_dl::x11 as xkbx;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        UTF8_STRING,
        TEXT,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _COMPTON_SHADOW,
        CLIPBOARD,
        TARGETS,
        INCR,
        QUI_PASTE,
        QUI_DROP,
        XdndAware,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndTypeList,
        XdndActionCopy,
    }
}

/// The XDND version spoken.
const XDND_VERSION: u32 = 5;

/// MIME types of clipboard content, as atoms are named.
const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];

/// A paste waiting on the clipboard's owner.
struct Paste {
    /// Text for the focused field, not everything.
    field: bool,
    /// What to read, once the owner listed its targets.
    plan: Option<PastePlan>,
    targets: Vec<Atom>,
    data: Vec<Vec<u8>>,
    /// Data arriving in chunks (INCR).
    incr: Option<Vec<u8>>,
    started: Instant,
}

/// A drag over the window, or a drop being read.
struct Drag {
    source: Window,
    /// Our window it is over.
    target: Window,
    files: bool,
    incr: Option<Vec<u8>>,
}

struct Win {
    id: Window,
    spec: Spec,
    /// Drawn over a transparent background (a 32-bit visual).
    transparent: bool,
    /// Shown (mapped, or being mapped); false while parked.
    shown: bool,
    /// The server mapped it: frames can be presented.
    mapped: bool,
    /// Physical size.
    size: (u32, u32),
    needs_attach: bool,
    /// The Vulkan surface must be created for it (new window).
    fresh: bool,
    grabbed: bool,
}

struct X {
    conn: XCBConnection,
    screen: usize,
    root: Window,
    atoms: Atoms,
    /// The clipboard and drop types qui reads or offers, by name.
    mimes: Vec<(&'static str, Atom)>,
    /// `_NET_WM_CM_Sn`, owned by a running compositing manager.
    cm_atom: Atom,
    /// A 32-bit visual and its colormap (made when first needed).
    argb: Option<(xproto::Visualid, Option<xproto::Colormap>)>,
    /// An unmapped window owning the clipboard and reading selections.
    helper: Window,
    win: Option<Win>,
    scale: f32,
    cursors: Option<x11rb::cursor::Handle>,
    loaded_cursors: [Option<xproto::Cursor>; 3],
    xkb_ctx: *mut xkb::xkb_context,
    xkb_device: i32,
    xkb: Option<Xkb>,
    /// The key last pressed and not released (a press of it again is a
    /// repeat).
    held: Option<u8>,
    /// The time of the last input event.
    time: xproto::Timestamp,
    trace: bool,

    events: Vec<Event>,
    fetched: Vec<Fetched>,
    woken: bool,
    focused: bool,
    focus_changed: Option<bool>,
    outside_click: bool,
    closed: bool,
    /// Exposed, mapped or otherwise needing a fresh frame.
    redraw: bool,
    pointer_inside: bool,
    cursor_dirty: bool,

    /// Text we put on the clipboard.
    clipboard: Option<String>,
    paste: Option<Paste>,
    drag: Option<Drag>,
    dropping: Option<Drag>,
}

fn cerr(e: impl std::fmt::Display) -> String {
    format!("X11: {e}")
}

/// Runs the app on X11 until [`Cx::exit`].
pub(crate) fn run<A: App>(make: impl FnOnce(Waker) -> A) -> Result<(), String> {
    let mut x = X::connect()?;
    let wake = Waker::new()?;
    let mut runner = Runner { app: make(wake.clone()), p: Painter::new() };
    let mut cx = Cx { scale: x.scale, ..Cx::default() };
    runner.app.start(&mut cx);
    if runner.apply(&mut x, cx)? {
        return Ok(());
    }
    loop {
        while let Some(event) = x.conn.poll_for_event().map_err(cerr)? {
            x.handle(event);
        }
        if runner.process(&mut x)? {
            return Ok(());
        }
        x.conn.flush().map_err(cerr)?;
        // Replies read while processing may have brought events along.
        if let Some(event) = x.conn.poll_for_event().map_err(cerr)? {
            x.handle(event);
            continue;
        }
        let mut fds = [
            libc::pollfd { fd: x.conn.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.0.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err("poll failed".into());
        }
        if fds[1].revents != 0 {
            wake.drain();
            x.woken = true;
        }
    }
}

struct Runner<A: App> {
    app: A,
    p: Painter,
}

impl<A: App> Runner<A> {
    /// Handles what the events produced. Returns true to exit.
    fn process(&mut self, x: &mut X) -> Result<bool, String> {
        let mut rebuild = false;
        let mut force = std::mem::take(&mut x.redraw);
        let mut cx = Cx { visible: x.visible(), scale: x.scale, ..Cx::default() };
        for f in std::mem::take(&mut x.fetched) {
            rebuild = true;
            self.p.fetched(&mut self.app, f);
        }
        if std::mem::take(&mut x.woken) && self.app.woken(&mut cx) {
            rebuild = true;
        }
        if let Some(focused) = x.focus_changed.take() {
            self.app.focus(focused, &mut cx);
            rebuild = true;
        }
        if std::mem::take(&mut x.outside_click) {
            self.app.clicked_outside(&mut cx);
            rebuild = true;
        }
        if std::mem::take(&mut x.closed) {
            self.teardown(x);
            self.app.closed(&mut cx);
        }
        let events = std::mem::take(&mut x.events);
        let (changed, paste) = self.p.events(&events);
        rebuild |= changed;
        if let Some(field) = paste {
            x.paste(field);
        }
        // A new or resized window: (re)attach the swapchain.
        if let Some(w) = &mut x.win
            && w.needs_attach
        {
            w.needs_attach = false;
            let native = Native::Xcb { connection: x.conn.get_raw_xcb_connection(), window: w.id };
            match &mut self.p.gfx {
                Some(gfx) if w.fresh => unsafe { gfx.attach_native(native, w.size)? },
                Some(gfx) => gfx.resize(w.size.0, w.size.1),
                None => {
                    let gfx = unsafe { Gfx::new(native, w.size, w.transparent) }?;
                    if self.p.stats {
                        eprintln!("qui: rendering on {} (X11)", gfx.device_name);
                    }
                    self.p.gfx = Some(gfx);
                }
            }
            w.fresh = false;
            if !w.shown
                && let Some(gfx) = &mut self.p.gfx
            {
                // Prepared: the swapchain now, the first frame on show.
                gfx.ensure_swapchain()?;
            }
            stat(format_args!("swapchain attached"));
            self.p.ensure_ui(&self.app)?;
            force = true;
        }
        let drawable = x.win.as_ref().is_some_and(|w| w.shown && w.mapped);
        if drawable && (force || rebuild || cx.again) && self.build(x, &mut cx, force) {
            self.p.dirty = true;
            stat(format_args!("built"));
        }
        if self.p.dirty && drawable {
            let transparent = x.win.as_ref().is_some_and(|w| w.transparent);
            self.p.draw(transparent)?;
        }
        self.apply(x, cx)
    }

    fn build(&mut self, x: &mut X, cx: &mut Cx, force: bool) -> bool {
        let Some(w) = &x.win else { return false };
        let logical = (w.size.0 as f32 / x.scale, w.size.1 as f32 / x.scale);
        let changed = self.p.build(&mut self.app, cx, logical, x.scale, force);
        if let Some(cursor) = self.p.cursor_change(&mut x.cursor_dirty) {
            x.set_cursor(cursor);
        }
        changed
    }

    /// Applies the app's requests. Returns true to exit.
    fn apply(&mut self, x: &mut X, cx: Cx) -> Result<bool, String> {
        if let Some(text) = cx.copy {
            x.copy(text);
        }
        if cx.paste {
            x.paste(false);
        }
        if cx.exit {
            self.teardown(x);
            let _ = x.conn.flush();
            return Ok(true);
        }
        if cx.hide && x.visible() {
            if !cx.release_gpu && x.win.as_ref().is_some_and(|w| w.spec.keep_when_hidden) {
                x.park();
                self.p.dirty = false;
            } else {
                self.teardown(x);
            }
        }
        if cx.release_gpu && !x.visible() {
            self.teardown(x);
            self.p.release_gpu();
        }
        if let Some(spec) = cx.prepare
            && x.win.is_none()
            && spec.keep_when_hidden
        {
            x.create(spec, false)?;
        }
        if let Some(spec) = cx.show {
            if !x.visible() {
                self.p.shown_at = Some(Instant::now());
                stat(format_args!("show"));
            }
            x.show(spec)?;
        }
        Ok(false)
    }

    fn teardown(&mut self, x: &mut X) {
        self.p.detach();
        x.destroy_window();
    }
}

impl X {
    fn connect() -> Result<X, String> {
        let (conn, screen) = XCBConnection::connect(None).map_err(|e| format!("Cannot connect to X11: {e}"))?;
        let atoms = Atoms::new(&conn).map_err(cerr)?;
        let names: Vec<&'static str> =
            ["text/uri-list"].into_iter().chain(IMAGE_EXTENSIONS.map(image_mime)).chain(TEXT_MIMES).collect();
        let cookies: Vec<_> =
            names.iter().map(|n| conn.intern_atom(false, n.as_bytes())).collect::<Result<_, _>>().map_err(cerr)?;
        let cm = conn.intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes()).map_err(cerr)?;
        let atoms = atoms.reply().map_err(cerr)?;
        let mut mimes = Vec::new();
        for (name, cookie) in names.into_iter().zip(cookies) {
            mimes.push((name, cookie.reply().map_err(cerr)?.atom));
        }
        let cm_atom = cm.reply().map_err(cerr)?.atom;

        let s = &conn.setup().roots[screen];
        let root = s.root;
        let argb = s
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| &d.visuals)
            .find(|v| v.class == xproto::VisualClass::TRUE_COLOR)
            .map(|v| (v.visual_id, None));

        let helper = conn.generate_id().map_err(cerr)?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            helper,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE).override_redirect(1),
        )
        .map_err(cerr)?;

        let db = x11rb::resource_manager::new_from_default(&conn).map_err(cerr)?;
        let dpi = db.get_value::<f32>("Xft.dpi", "").ok().flatten().filter(|d| *d > 0.0).unwrap_or(96.0);
        let scale = ((dpi / 96.0 * 120.0).round() / 120.0).max(0.5);
        let cursors = x11rb::cursor::Handle::new(&conn, screen, &db).ok().and_then(|c| c.reply().ok());

        let mut x = X {
            conn,
            screen,
            root,
            atoms,
            mimes,
            cm_atom,
            argb,
            helper,
            win: None,
            scale,
            cursors,
            loaded_cursors: [None; 3],
            xkb_ctx: std::ptr::null_mut(),
            xkb_device: -1,
            xkb: None,
            held: None,
            time: CURRENT_TIME,
            trace: std::env::var_os("QUI_EVENTS").is_some(),
            events: Vec::new(),
            fetched: Vec::new(),
            woken: false,
            focused: false,
            focus_changed: None,
            outside_click: false,
            closed: false,
            redraw: false,
            pointer_inside: false,
            cursor_dirty: false,
            clipboard: None,
            paste: None,
            drag: None,
            dropping: None,
        };
        stat(format_args!("X11 screen {screen}, scale {scale}"));
        if !x.setup_keyboard() {
            eprintln!("qui: libxkbcommon-x11 or the XKB extension is missing; keyboard input won't work");
        }
        Ok(x)
    }

    /// Loads the keymap and asks for its changes and the modifier state.
    fn setup_keyboard(&mut self) -> bool {
        let (Some(x), Some(xx)) = (xkb::xkbcommon_option(), xkbx::xkbcommon_x11_option()) else { return false };
        let raw = self.conn.get_raw_xcb_connection();
        let ok = unsafe {
            (xx.xkb_x11_setup_xkb_extension)(
                raw,
                xkbx::XKB_X11_MIN_MAJOR_XKB_VERSION,
                xkbx::XKB_X11_MIN_MINOR_XKB_VERSION,
                xkbx::xkb_x11_setup_xkb_extension_flags::XKB_X11_SETUP_XKB_EXTENSION_NO_FLAGS,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 1 {
            return false;
        }
        self.xkb_device = unsafe { (xx.xkb_x11_get_core_keyboard_device_id)(raw) };
        self.xkb_ctx = unsafe { (x.xkb_context_new)(xkb::xkb_context_flags::XKB_CONTEXT_NO_FLAGS) };
        if self.xkb_device < 0 || self.xkb_ctx.is_null() {
            return false;
        }
        let events = xxkb::EventType::NEW_KEYBOARD_NOTIFY | xxkb::EventType::MAP_NOTIFY | xxkb::EventType::STATE_NOTIFY;
        let parts = xxkb::MapPart::KEY_TYPES
            | xxkb::MapPart::KEY_SYMS
            | xxkb::MapPart::MODIFIER_MAP
            | xxkb::MapPart::EXPLICIT_COMPONENTS
            | xxkb::MapPart::KEY_ACTIONS
            | xxkb::MapPart::VIRTUAL_MODS
            | xxkb::MapPart::VIRTUAL_MOD_MAP;
        let device = self.xkb_device as xxkb::DeviceSpec;
        let _ = self.conn.xkb_select_events(
            device,
            xxkb::EventType::from(0u16),
            events,
            parts,
            parts,
            &xxkb::SelectEventsAux::new(),
        );
        // Repeats as presses without releases in between.
        let _ = self.conn.xkb_per_client_flags(
            xxkb::ID::USE_CORE_KBD.into(),
            xxkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            xxkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            xxkb::BoolCtrl::from(0u32),
            xxkb::BoolCtrl::from(0u32),
            xxkb::BoolCtrl::from(0u32),
        );
        self.load_keymap();
        self.xkb.is_some()
    }

    fn load_keymap(&mut self) {
        let Some(xx) = xkbx::xkbcommon_x11_option() else { return };
        let raw = self.conn.get_raw_xcb_connection();
        let keymap = unsafe {
            (xx.xkb_x11_keymap_new_from_device)(
                self.xkb_ctx,
                raw,
                self.xkb_device,
                xkb::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        if keymap.is_null() {
            return;
        }
        let state = unsafe { (xx.xkb_x11_state_new_from_device)(keymap, raw, self.xkb_device) };
        if state.is_null() {
            if let Some(x) = xkb::xkbcommon_option() {
                unsafe { (x.xkb_keymap_unref)(keymap) };
            }
            return;
        }
        self.xkb = Some(Xkb { keymap, state });
    }

    fn atom(&self, mime: &str) -> Atom {
        self.mimes.iter().find(|(n, _)| *n == mime).map_or(NONE, |&(_, a)| a)
    }

    fn mime(&self, atom: Atom) -> Option<&'static str> {
        self.mimes.iter().find(|(_, a)| *a == atom).map(|&(n, _)| n)
    }

    /// Whether a window is shown (not parked).
    fn visible(&self) -> bool {
        self.win.as_ref().is_some_and(|w| w.shown)
    }

    fn compositing(&self) -> bool {
        self.conn
            .get_selection_owner(self.cm_atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.owner != NONE)
    }

    fn physical(&self, (w, h): (u32, u32)) -> (u32, u32) {
        let p = |v: u32| ((v as f32 * self.scale).round() as u32).max(1);
        (p(w), p(h))
    }

    /// The monitor under the pointer (else the primary, else the screen):
    /// x, y, width, height.
    fn monitor(&self) -> (i32, i32, i32, i32) {
        let s = &self.conn.setup().roots[self.screen];
        let screen = (0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32);
        let pointer = self.conn.query_pointer(self.root).ok().and_then(|c| c.reply().ok());
        let Some(monitors) = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok()) else {
            return screen;
        };
        let rect = |m: &x11rb::protocol::randr::MonitorInfo| (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
        let under = pointer.and_then(|p| {
            let (px, py) = (p.root_x as i32, p.root_y as i32);
            monitors.monitors.iter().map(rect).find(|&(x, y, w, h)| px >= x && py >= y && px < x + w && py < y + h)
        });
        under
            .or_else(|| monitors.monitors.iter().find(|m| m.primary).map(rect))
            .or_else(|| monitors.monitors.first().map(rect))
            .unwrap_or(screen)
    }

    /// Creates the window for `spec`, mapped if `show` (else parked, as
    /// [`Cx::prepare`] asks).
    fn create(&mut self, spec: Spec, show: bool) -> Result<(), String> {
        let transparent = spec.transparent && self.argb.is_some() && self.compositing();
        let size = self.physical((spec.width, spec.height));
        let (x, y) = if spec.layer_shell { place(&spec, self.scale, size, self.monitor()) } else { (0, 0) };
        let id = self.conn.generate_id().map_err(cerr)?;
        let mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW
            | EventMask::FOCUS_CHANGE;
        let mut aux =
            CreateWindowAux::new().event_mask(mask).override_redirect(u32::from(spec.layer_shell)).border_pixel(0);
        let (depth, visual) = match self.argb {
            Some((visual, ref mut colormap)) if transparent => {
                let cmap = match *colormap {
                    Some(c) => c,
                    None => {
                        let c = self.conn.generate_id().map_err(cerr)?;
                        self.conn.create_colormap(xproto::ColormapAlloc::NONE, c, self.root, visual).map_err(cerr)?;
                        *colormap = Some(c);
                        c
                    }
                };
                aux = aux.colormap(cmap).background_pixel(0);
                (32, visual)
            }
            _ => (COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT),
        };
        self.conn
            .create_window(
                depth,
                id,
                self.root,
                x as i16,
                y as i16,
                size.0 as u16,
                size.1 as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &aux,
            )
            .map_err(cerr)?;
        stat(format_args!(
            "window {}x{} at {x},{y}{}{}",
            size.0,
            size.1,
            if spec.layer_shell { ", override-redirect" } else { "" },
            if transparent { ", 32-bit visual" } else { "" },
        ));
        self.win = Some(Win {
            id,
            spec,
            transparent,
            shown: false,
            mapped: false,
            size,
            needs_attach: true,
            fresh: true,
            grabbed: false,
        });
        self.set_properties();
        self.cursor_dirty = true;
        if show {
            self.map();
        }
        Ok(())
    }

    /// Sets the window's title, class, type and hints from its spec.
    fn set_properties(&self) {
        let Some(w) = &self.win else { return };
        let (c, a, spec) = (&self.conn, &self.atoms, &w.spec);
        let _ = c.change_property8(PropMode::REPLACE, w.id, AtomEnum::WM_NAME, AtomEnum::STRING, spec.title.as_bytes());
        let _ = c.change_property8(PropMode::REPLACE, w.id, a._NET_WM_NAME, a.UTF8_STRING, spec.title.as_bytes());
        let class = if spec.layer_shell { &spec.namespace } else { &spec.app_id };
        let class = format!("{class}\0{class}\0");
        let _ = c.change_property8(PropMode::REPLACE, w.id, AtomEnum::WM_CLASS, AtomEnum::STRING, class.as_bytes());
        let _ = c.change_property32(PropMode::REPLACE, w.id, a.WM_PROTOCOLS, AtomEnum::ATOM, &[a.WM_DELETE_WINDOW]);
        let _ = c.change_property32(PropMode::REPLACE, w.id, a._NET_WM_PID, AtomEnum::CARDINAL, &[std::process::id()]);
        let _ = c.change_property32(PropMode::REPLACE, w.id, a.XdndAware, AtomEnum::ATOM, &[XDND_VERSION]);
        let kind = match (spec.layer_shell, spec.layer) {
            (true, Layer::Background) => a._NET_WM_WINDOW_TYPE_DESKTOP,
            (true, Layer::Bottom | Layer::Top) => a._NET_WM_WINDOW_TYPE_DOCK,
            _ => a._NET_WM_WINDOW_TYPE_NORMAL,
        };
        let _ = c.change_property32(PropMode::REPLACE, w.id, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, &[kind]);
        if w.transparent {
            // A compositor's shadow would outline the transparent margin.
            let _ = c.change_property32(PropMode::REPLACE, w.id, a._COMPTON_SHADOW, AtomEnum::CARDINAL, &[0]);
        }
        let mut hints = WmSizeHints::new();
        if spec.resizable {
            let (mw, mh) = self.physical((spec.min_size.0.max(1), spec.min_size.1.max(1)));
            hints.min_size = Some((mw as i32, mh as i32));
        } else {
            hints.min_size = Some((w.size.0 as i32, w.size.1 as i32));
            hints.max_size = hints.min_size;
        }
        let _ = hints.set_normal_hints(c, w.id);
        let wm = WmHints { input: Some(!spec.layer_shell || spec.keyboard != Keyboard::None), ..WmHints::new() };
        let _ = wm.set(c, w.id);
    }

    fn map(&mut self) {
        let Some(w) = &mut self.win else { return };
        w.shown = true;
        if w.spec.layer_shell {
            let stack = match w.spec.layer {
                Layer::Top | Layer::Overlay => StackMode::ABOVE,
                Layer::Background | Layer::Bottom => StackMode::BELOW,
            };
            let _ = self.conn.configure_window(w.id, &ConfigureWindowAux::new().stack_mode(stack));
        }
        let _ = self.conn.map_window(w.id);
    }

    /// Shows `spec`: a new window, or the existing one (parked or shown)
    /// updated.
    fn show(&mut self, spec: Spec) -> Result<(), String> {
        let Some(w) = &self.win else { return self.create(spec, true) };
        let same_kind = w.spec.layer_shell == spec.layer_shell && w.spec.transparent == spec.transparent;
        if !same_kind {
            self.destroy_window();
            return self.create(spec, true);
        }
        if w.shown && w.spec == spec {
            return Ok(());
        }
        let resized = (w.spec.width, w.spec.height) != (spec.width, spec.height);
        let size = self.physical((spec.width, spec.height));
        let at = spec.layer_shell.then(|| place(&spec, self.scale, size, self.monitor()));
        let Some(w) = &mut self.win else { return Ok(()) };
        w.spec = spec;
        let mut aux = ConfigureWindowAux::new();
        if let Some((x, y)) = at {
            aux = aux.x(x).y(y);
        }
        if resized {
            aux = aux.width(size.0).height(size.1);
            if w.spec.layer_shell {
                // Not managed: the size is ours to set.
                w.size = size;
                w.needs_attach = true;
            }
        }
        let _ = self.conn.configure_window(w.id, &aux);
        let shown = w.shown;
        self.set_properties();
        if !shown {
            self.map();
        }
        Ok(())
    }

    /// Hides the window but keeps it (and its swapchain).
    fn park(&mut self) {
        self.ungrab();
        self.lose_input();
        if let Some(w) = &mut self.win {
            w.shown = false;
            let _ = self.conn.unmap_window(w.id);
        }
    }

    fn destroy_window(&mut self) {
        self.ungrab();
        self.lose_input();
        if let Some(w) = self.win.take() {
            let _ = self.conn.destroy_window(w.id);
        }
        self.drag = None;
    }

    /// The window stops getting input (hidden or gone).
    fn lose_input(&mut self) {
        self.held = None;
        self.pointer_inside = false;
        if std::mem::take(&mut self.focused) {
            self.focus_changed = Some(false);
            self.events.push(Event::Focus(false));
            self.events.push(Event::Modifiers(Mods::NONE));
        }
    }

    /// Takes the keyboard (and the pointer, for clicks outside) for an
    /// exclusive layer window, or the focus for an on-demand one.
    fn take_input(&mut self) {
        let Some(w) = &self.win else { return };
        if !w.spec.layer_shell {
            return;
        }
        let id = w.id;
        match w.spec.keyboard {
            Keyboard::None => {}
            Keyboard::OnDemand => {
                let _ = self.conn.set_input_focus(InputFocus::PARENT, id, CURRENT_TIME);
            }
            Keyboard::Exclusive => {
                // Another client (a hotkey daemon) may still hold the
                // keyboard for a moment after launching us.
                let grab = |x: &X| {
                    x.conn
                        .grab_keyboard(true, id, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .is_some_and(|r| r.status == GrabStatus::SUCCESS)
                };
                let mut ok = grab(self);
                for _ in 0..25 {
                    if ok {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    ok = grab(self);
                }
                if !ok {
                    eprintln!("qui: cannot grab the keyboard");
                    return;
                }
                let mask = EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW;
                let _ = self.conn.grab_pointer(true, id, mask, GrabMode::ASYNC, GrabMode::ASYNC, NONE, NONE, CURRENT_TIME);
                if let Some(w) = &mut self.win {
                    w.grabbed = true;
                }
                self.set_focus(true);
            }
        }
    }

    fn ungrab(&mut self) {
        if let Some(w) = &mut self.win
            && std::mem::take(&mut w.grabbed)
        {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        }
    }

    fn set_focus(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.focus_changed = Some(focused);
        self.events.push(Event::Focus(focused));
        if !focused {
            self.held = None;
            self.events.push(Event::Modifiers(Mods::NONE));
        }
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        let (Some(w), Some(handle)) = (&self.win, &self.cursors) else { return };
        let (i, name) = match cursor {
            Cursor::Default => (0, "left_ptr"),
            Cursor::Pointer => (1, "hand2"),
            Cursor::Text => (2, "xterm"),
        };
        let id = match self.loaded_cursors[i] {
            Some(id) => id,
            None => {
                let Ok(id) = handle.load_cursor(&self.conn, name) else { return };
                self.loaded_cursors[i] = Some(id);
                id
            }
        };
        let _ = self.conn.change_window_attributes(w.id, &ChangeWindowAttributesAux::new().cursor(id));
    }

    fn is_win(&self, id: Window) -> bool {
        self.win.as_ref().is_some_and(|w| w.id == id)
    }

    fn inside(&self, x: i16, y: i16) -> bool {
        self.win.as_ref().is_some_and(|w| x >= 0 && y >= 0 && (x as u32) < w.size.0 && (y as u32) < w.size.1)
    }

    fn handle(&mut self, event: XEvent) {
        if self.trace {
            match &event {
                XEvent::MotionNotify(_) => {}
                XEvent::FocusIn(e) | XEvent::FocusOut(e) => {
                    let kind = if matches!(event, XEvent::FocusIn(_)) { "FocusIn" } else { "FocusOut" };
                    eprintln!("qui: x11 {kind} {:#x} {:?} {:?}", e.event, e.mode, e.detail);
                }
                _ => eprintln!("qui: x11 {event:?}"),
            }
        }
        match event {
            XEvent::Expose(e) if e.count == 0 && self.is_win(e.window) => self.redraw = true,
            XEvent::MapNotify(e) if self.is_win(e.window) => {
                if let Some(w) = &mut self.win {
                    w.mapped = true;
                }
                self.redraw = true;
                self.take_input();
            }
            XEvent::UnmapNotify(e) if self.is_win(e.window) => {
                if let Some(w) = &mut self.win {
                    w.mapped = false;
                }
            }
            XEvent::ConfigureNotify(e) if self.is_win(e.window) => {
                if let Some(w) = &mut self.win {
                    let size = ((e.width as u32).max(1), (e.height as u32).max(1));
                    if size != w.size {
                        w.size = size;
                        w.needs_attach = true;
                    }
                }
            }
            XEvent::DestroyNotify(e) if self.is_win(e.window) => {
                self.win = None;
                self.closed = true;
            }
            XEvent::ClientMessage(e) => self.client_message(e),
            XEvent::KeyPress(e) => {
                self.time = e.time;
                let repeat = self.held == Some(e.detail);
                self.held = Some(e.detail);
                if let Some((mut key, repeats)) = self.xkb.as_ref().and_then(|k| k.key(e.detail as u32))
                    && (repeats || !repeat)
                {
                    key.repeat = repeat;
                    self.events.push(Event::Key(key));
                }
            }
            XEvent::KeyRelease(e) => {
                self.time = e.time;
                if self.held == Some(e.detail) {
                    self.held = None;
                }
            }
            XEvent::ButtonPress(e) if self.is_win(e.event) => {
                self.time = e.time;
                if !self.inside(e.event_x, e.event_y) {
                    self.outside_click = true;
                    return;
                }
                let event = match e.detail {
                    1 => Event::Button { button: Button::Left, pressed: true },
                    2 => Event::Button { button: Button::Middle, pressed: true },
                    3 => Event::Button { button: Button::Right, pressed: true },
                    4 => Event::WheelLines(1.0),
                    5 => Event::WheelLines(-1.0),
                    6 | 7 => return,
                    _ => Event::Button { button: Button::Other, pressed: true },
                };
                self.events.push(event);
            }
            XEvent::ButtonRelease(e) if self.is_win(e.event) => {
                self.time = e.time;
                let button = match e.detail {
                    1 => Button::Left,
                    2 => Button::Middle,
                    3 => Button::Right,
                    4..=7 => return,
                    _ => Button::Other,
                };
                self.events.push(Event::Button { button, pressed: false });
            }
            XEvent::MotionNotify(e) if self.is_win(e.event) => {
                if self.inside(e.event_x, e.event_y) {
                    self.pointer_inside = true;
                    self.events.push(Event::PointerMoved(e.event_x as f32, e.event_y as f32));
                } else if std::mem::take(&mut self.pointer_inside) {
                    self.events.push(Event::PointerLeft);
                }
            }
            XEvent::EnterNotify(e) if self.is_win(e.event) && self.inside(e.event_x, e.event_y) => {
                self.pointer_inside = true;
                self.cursor_dirty = true;
                self.events.push(Event::PointerMoved(e.event_x as f32, e.event_y as f32));
                // The cursor is set on the next build.
                self.woken = true;
            }
            XEvent::LeaveNotify(e) if self.is_win(e.event) && e.mode == NotifyMode::NORMAL => {
                if std::mem::take(&mut self.pointer_inside) {
                    self.events.push(Event::PointerLeft);
                }
            }
            XEvent::FocusIn(e) if self.is_win(e.event) && e.mode != NotifyMode::UNGRAB => self.set_focus(true),
            XEvent::FocusOut(e) if self.is_win(e.event) && e.mode != NotifyMode::GRAB => {
                // A grab of ours keeps the keyboard.
                if !self.win.as_ref().is_some_and(|w| w.grabbed) {
                    self.set_focus(false);
                }
            }
            XEvent::XkbStateNotify(e) => {
                if let (Some(x), Some(k)) = (xkb::xkbcommon_option(), &self.xkb) {
                    unsafe {
                        (x.xkb_state_update_mask)(
                            k.state,
                            u16::from(e.base_mods) as u32,
                            u16::from(e.latched_mods) as u32,
                            u16::from(e.locked_mods) as u32,
                            e.base_group as u32,
                            e.latched_group as u32,
                            u8::from(e.locked_group) as u32,
                        )
                    };
                    let mods = k.mods();
                    self.events.push(Event::Modifiers(mods));
                }
            }
            XEvent::XkbMapNotify(_) => self.load_keymap(),
            XEvent::XkbNewKeyboardNotify(e) if e.device_id as i32 == self.xkb_device => self.load_keymap(),
            XEvent::SelectionRequest(e) => self.serve(e),
            XEvent::SelectionClear(e) if e.selection == self.atoms.CLIPBOARD => self.clipboard = None,
            XEvent::SelectionNotify(e) if e.requestor == self.helper => self.selection_notify(e),
            XEvent::PropertyNotify(e) if e.window == self.helper && e.state == xproto::Property::NEW_VALUE => {
                self.incr_chunk(e.atom);
            }
            _ => {}
        }
    }

    // ---- the clipboard -------------------------------------------------------------

    fn copy(&mut self, text: String) {
        let _ = self.conn.set_selection_owner(self.helper, self.atoms.CLIPBOARD, self.time);
        self.clipboard = Some(text);
    }

    /// Answers another client's request for our clipboard text.
    fn serve(&mut self, e: xproto::SelectionRequestEvent) {
        let a = &self.atoms;
        let property = if e.property == NONE { e.target } else { e.property };
        let texts: Vec<Atom> = TEXT_MIMES.iter().map(|m| self.atom(m)).collect();
        let ok = match &self.clipboard {
            Some(_) if e.selection == a.CLIPBOARD && e.target == a.TARGETS => {
                let mut targets = vec![a.TARGETS];
                targets.extend(&texts);
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &targets).is_ok()
            }
            Some(text)
                if e.selection == a.CLIPBOARD
                    && texts.contains(&e.target)
                    && text.len() + 64 < self.conn.maximum_request_bytes() =>
            {
                let kind = if e.target == a.TEXT { a.UTF8_STRING } else { e.target };
                self.conn.change_property8(PropMode::REPLACE, e.requestor, property, kind, text.as_bytes()).is_ok()
            }
            _ => false,
        };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { NONE },
        };
        let _ = self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
    }

    /// Reads the clipboard: the text for the focused field, or everything.
    fn paste(&mut self, field: bool) {
        if self.paste.as_ref().is_some_and(|p| p.started.elapsed() < Duration::from_secs(5)) {
            return;
        }
        let a = &self.atoms;
        let _ = self.conn.convert_selection(self.helper, a.CLIPBOARD, a.TARGETS, a.QUI_PASTE, self.time);
        self.paste =
            Some(Paste { field, plan: None, targets: Vec::new(), data: Vec::new(), incr: None, started: Instant::now() });
    }

    /// Reads (and deletes) a property of the helper window: its data, or
    /// `None` when the owner sends it in chunks.
    fn take_property(&self, property: Atom) -> Option<Vec<u8>> {
        let reply = self
            .conn
            .get_property(true, self.helper, property, AtomEnum::ANY, 0, u32::MAX / 4)
            .ok()
            .and_then(|c| c.reply().ok());
        match reply {
            Some(r) if r.type_ == self.atoms.INCR => None,
            Some(r) => Some(r.value),
            None => Some(Vec::new()),
        }
    }

    fn selection_notify(&mut self, e: xproto::SelectionNotifyEvent) {
        let a = &self.atoms;
        let (selection, property) = (e.selection, e.property);
        let data = if property == NONE { Some(Vec::new()) } else { self.take_property(property) };
        if selection == a.CLIPBOARD && self.paste.is_some() {
            match data {
                Some(data) => self.pasted(data),
                None => self.paste.as_mut().unwrap().incr = Some(Vec::new()),
            }
        } else if selection == a.XdndSelection && self.dropping.is_some() {
            match data {
                Some(data) => self.dropped(data),
                None => self.dropping.as_mut().unwrap().incr = Some(Vec::new()),
            }
        }
    }

    /// A chunk of an INCR transfer arrived.
    fn incr_chunk(&mut self, property: Atom) {
        let a = &self.atoms;
        let receiving = |incr: &Option<Vec<u8>>| incr.is_some();
        let paste = property == a.QUI_PASTE && self.paste.as_ref().is_some_and(|p| receiving(&p.incr));
        let drop = property == a.QUI_DROP && self.dropping.as_ref().is_some_and(|d| receiving(&d.incr));
        if !paste && !drop {
            return;
        }
        let chunk = self.take_property(property).unwrap_or_default();
        let incr = if paste {
            self.paste.as_mut().and_then(|p| p.incr.as_mut())
        } else {
            self.dropping.as_mut().and_then(|d| d.incr.as_mut())
        };
        let Some(buf) = incr else { return };
        if !chunk.is_empty() {
            buf.extend_from_slice(&chunk);
            return;
        }
        let data = std::mem::take(buf);
        if paste {
            self.paste.as_mut().unwrap().incr = None;
            self.pasted(data);
        } else {
            self.dropped(data);
        }
    }

    /// One form of the clipboard's content was read (or its targets).
    fn pasted(&mut self, data: Vec<u8>) {
        let Some(mut p) = self.paste.take() else { return };
        if p.plan.is_none() && p.targets.is_empty() {
            // The owner's targets.
            let mimes: Vec<String> = data
                .as_chunks::<4>()
                .0
                .iter()
                .filter_map(|&b| self.mime(u32::from_ne_bytes(b)))
                .map(str::to_string)
                .collect();
            if p.field {
                let Some(mime) = text_mime(&mimes) else { return };
                p.targets.push(self.atom(mime));
            } else {
                let plan = PastePlan::new(&mimes);
                p.targets = plan.mimes().map(|m| self.atom(m)).collect();
                p.plan = Some(plan);
            }
            if p.targets.is_empty() {
                self.fetched.push(Fetched::Pasted(Default::default()));
                return;
            }
        } else {
            p.data.push(data);
        }
        if let Some(&target) = p.targets.get(p.data.len()) {
            let a = &self.atoms;
            let _ = self.conn.convert_selection(self.helper, a.CLIPBOARD, target, a.QUI_PASTE, self.time);
            self.paste = Some(p);
            return;
        }
        self.fetched.push(match p.plan {
            Some(plan) => Fetched::Pasted(plan.assemble(p.data)),
            None => Fetched::Field(String::from_utf8_lossy(&p.data.remove(0)).into_owned()),
        });
    }

    // ---- drag and drop (XDND) ---------------------------------------------------------

    fn client_message(&mut self, e: ClientMessageEvent) {
        let a = &self.atoms;
        let d = e.data.as_data32();
        if e.type_ == a.WM_PROTOCOLS && d[0] == a.WM_DELETE_WINDOW && self.is_win(e.window) {
            self.closed = true;
        } else if e.type_ == a.XdndEnter && self.is_win(e.window) {
            let source = d[0];
            let types = if d[1] & 1 != 0 {
                self.conn
                    .get_property(false, source, a.XdndTypeList, AtomEnum::ATOM, 0, 1024)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().map(|v| v.collect()))
                    .unwrap_or_default()
            } else {
                d[2..5].to_vec()
            };
            let files = types.contains(&self.atom("text/uri-list"));
            self.drag = Some(Drag { source, target: e.window, files, incr: None });
        } else if e.type_ == a.XdndPosition && self.is_win(e.window) {
            let Some(drag) = &self.drag else { return };
            let accept = drag.files;
            let action = if accept { a.XdndActionCopy } else { NONE };
            self.send_xdnd(drag.source, a.XdndStatus, [e.window, u32::from(accept), 0, 0, action]);
        } else if e.type_ == a.XdndLeave {
            self.drag = None;
        } else if e.type_ == a.XdndDrop {
            let Some(drag) = self.drag.take() else { return };
            if drag.files {
                let time = d[2];
                let uris = self.atom("text/uri-list");
                let _ = self.conn.convert_selection(self.helper, a.XdndSelection, uris, a.QUI_DROP, time);
                self.dropping = Some(drag);
            } else {
                self.send_xdnd(drag.source, a.XdndFinished, [drag.target, 0, 0, 0, 0]);
            }
        }
    }

    fn send_xdnd(&self, to: Window, kind: Atom, data: [u32; 5]) {
        let event = ClientMessageEvent::new(32, to, kind, data);
        let _ = self.conn.send_event(false, to, EventMask::NO_EVENT, event);
    }

    /// The dropped URI list was read.
    fn dropped(&mut self, data: Vec<u8>) {
        let Some(drag) = self.dropping.take() else { return };
        let paths = uri_paths(&uri_list(&data));
        let a = &self.atoms;
        let accepted = !paths.is_empty();
        let action = if accepted { a.XdndActionCopy } else { NONE };
        self.send_xdnd(drag.source, a.XdndFinished, [drag.target, u32::from(accepted), action, 0, 0]);
        self.fetched.push(Fetched::Dropped(paths));
    }
}

impl Drop for X {
    fn drop(&mut self) {
        self.xkb = None;
        if !self.xkb_ctx.is_null()
            && let Some(x) = xkb::xkbcommon_option()
        {
            unsafe { (x.xkb_context_unref)(self.xkb_ctx) };
        }
    }
}
