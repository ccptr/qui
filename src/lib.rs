//! `vkui`: a small immediate-mode GUI toolkit for one window.
//!
//! * [`font`]: TrueType parsing and rasterization (no font crates).
//! * [`text`]: glyph cache and atlas, measuring, eliding, wrapping.
//! * [`gfx`]: Vulkan through `ash`: one pipeline of instanced quads.
//! * [`ui`]: widgets, layout helpers, input handling.
//!
//! [`run`] drives an [`App`] with winit. It is event-driven: the UI is
//! rebuilt after each input or app event, and a frame is only rendered when
//! the resulting quads differ from the last frame. An idle window wakes up
//! for nothing.

pub mod font;
pub mod gfx;
pub mod text;
pub mod ui;

pub use ui::{Align, ButtonStyle, Color, Edit, Rect, Response, Theme, Ui, id};
pub use winit;

use gfx::{Batch, Gfx, Quad};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::data_transfer::{DataTransferId, DataTransferSendBuilder, TransferType, TypeHint, TypedData};
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, AsyncRequestSerial, ControlFlow, DndAction, EventLoop, EventLoopProxy};
use winit::cursor::CursorIcon;
use winit::window::{
    ImeCapabilities, ImeEnableRequest, ImeHint, ImePurpose, ImeRequest, ImeRequestData, Window,
    WindowAttributes, WindowId,
};

/// Requests from the app to the runner during [`App::build`].
#[derive(Default)]
pub struct Cx {
    pub close: bool,
    /// The app changed its state while building (e.g. a click opened a
    /// dialog), so parts drawn earlier are stale: build once more. The
    /// input was consumed by the first pass.
    pub again: bool,
    /// Put this text on the clipboard.
    pub copy: Option<String>,
    /// Paste as if Ctrl+V was pressed outside a text field: see
    /// [`App::pasted`].
    pub paste: bool,
}

/// What the clipboard held when pasting outside a text field. Programs
/// often offer one thing in several forms (a file manager: the files, and
/// their paths as text); a form that only repeats another is left out, but
/// different things offered together are all here: which of them the user
/// meant can't be told from the clipboard.
#[derive(Debug, Default)]
pub struct Pasted {
    /// Copied local files and folders.
    pub files: Vec<PathBuf>,
    /// A copied image: its encoded bytes, and the file extension of its
    /// format ("png", "jpg", ...).
    pub image: Option<(Vec<u8>, &'static str)>,
    /// Copied text (never blank).
    pub text: Option<String>,
}

impl Pasted {
    /// How many kinds of content (files, image, text) there are.
    pub fn kinds(&self) -> usize {
        usize::from(!self.files.is_empty()) + usize::from(self.image.is_some()) + usize::from(self.text.is_some())
    }

    /// Leaves out forms that repeat the files: text that only lists them
    /// (as paths or URIs), and an image that is the one copied file's
    /// content.
    fn drop_repeats(&mut self, uris: &[String]) {
        if self.files.is_empty() {
            return;
        }
        let files = &self.files;
        let is_file = |line: &str| {
            uris.iter().any(|uri| uri.trim() == line)
                || files.iter().any(|path| path.as_os_str() == line)
                || uri_path(line).is_some_and(|path| files.contains(&path))
        };
        if self.text.as_deref().is_some_and(|text| {
            text.lines().map(str::trim).filter(|line| !line.is_empty()).all(is_file)
        }) {
            self.text = None;
        }
        if let (Some((data, _)), [file]) = (&self.image, files.as_slice())
            && std::fs::metadata(file).is_ok_and(|meta| meta.len() == data.len() as u64)
            && std::fs::read(file).is_ok_and(|content| content == *data)
        {
            self.image = None;
        }
    }
}

pub trait App: 'static {
    fn title(&self) -> String;

    /// Wayland app id / X11 class, matched against `.desktop` files.
    fn app_id(&self) -> &str {
        "vkui"
    }

    /// Builds the whole UI for this frame.
    fn build(&mut self, ui: &mut Ui, cx: &mut Cx);

    /// Something woke the loop through the [`EventLoopProxy`] (e.g. "the
    /// engine state changed"); return whether the UI must be rebuilt.
    /// Wake-ups are coalesced: several `wake_up` calls may arrive as one.
    fn woken(&mut self) -> bool;

    /// A file was dropped onto the window.
    fn dropped(&mut self, _path: PathBuf) {}

    /// Ctrl+V (or [`Cx::paste`]) outside a text field: everything the
    /// clipboard held, possibly nothing. With several kinds of content, the
    /// app asks which to take.
    fn pasted(&mut self, _content: Pasted) {}

    /// Copying to or pasting from the clipboard failed.
    fn clipboard_error(&mut self, _error: String) {}
}

/// What a pending `fetch_data_transfer` is for.
#[derive(Clone, Copy, Debug)]
enum Fetch {
    /// The `text/uri-list` of a drop.
    Drop,
    /// Text for the focused text field.
    Field,
    /// One form of a paste outside a text field, collected in [`Gather`].
    Files,
    Image { extension: &'static str },
    Text,
}

impl Fetch {
    fn is_gather(&self) -> bool {
        matches!(self, Fetch::Files | Fetch::Image { .. } | Fetch::Text)
    }
}

/// A paste outside a text field: every form the clipboard offers is read,
/// then handed over at once as a [`Pasted`].
#[derive(Default)]
struct Gather {
    /// Forms still being read.
    waiting: usize,
    uris: Vec<String>,
    image: Option<(Vec<u8>, &'static str)>,
    text: Option<String>,
    /// The last read that failed.
    error: Option<String>,
}

/// Image formats taken from the clipboard, most preferred first. Programs
/// that copy an image usually offer several; PNG is lossless and readable
/// everywhere.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "webp", "gif", "bmp", "tiff", "svg", "ico"];

/// The most preferred image format the clipboard offers.
fn image_type(dt: &dyn winit::data_transfer::DataTransfer) -> Option<&'static str> {
    let types = dt.available_types();
    IMAGE_EXTENSIONS.into_iter().find(|&ext| {
        types
            .iter()
            .any(|ty| ty.hint() == Some(TypeHint::Image { extension_hint: Some(ext) }))
    })
}

struct Runner<A: App> {
    app: A,
    window: Option<Box<dyn Window>>,
    gfx: Option<Gfx>,
    ui: Option<Ui>,
    /// The last built frame, drawn on redraw.
    quads: Vec<Quad>,
    batches: Vec<Batch>,
    cursor: CursorIcon,
    /// Shared with [`run`], which gets the handler back only by value.
    out: Rc<RefCell<Outcome>>,
    /// Drop and clipboard requests, answered by `DataTransferReceived`.
    fetches: Vec<(AsyncRequestSerial, Fetch)>,
    gather: Gather,
    frames: u64,
    builds: u64,
    started: std::time::Instant,
    stats: bool,
    /// RLS_EVENTS: log every wakeup and event (to find idle wakeups).
    trace: bool,
}

#[derive(Default)]
struct Outcome {
    error: Option<String>,
    builds: u64,
    frames: u64,
}

/// Opens the window and runs until it is closed. `make` gets a proxy whose
/// `wake_up` calls [`App::woken`] on the UI thread; it can be used from any
/// thread.
pub fn run<A: App>(make: impl FnOnce(EventLoopProxy) -> A) -> Result<(), String> {
    // RLS_STATS times frames from here, so `make` (the app's own start-up)
    // counts too.
    let started = std::time::Instant::now();
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let app = make(event_loop.create_proxy());
    let out = Rc::new(RefCell::new(Outcome::default()));
    let stats = std::env::var_os("RLS_STATS").is_some();
    let runner = Runner {
        app,
        window: None,
        gfx: None,
        ui: None,
        quads: Vec::new(),
        batches: Vec::new(),
        cursor: CursorIcon::Default,
        out: out.clone(),
        fetches: Vec::new(),
        gather: Gather::default(),
        frames: 0,
        builds: 0,
        started,
        stats,
        trace: std::env::var_os("RLS_EVENTS").is_some(),
    };
    event_loop.run_app(runner).map_err(|e| e.to_string())?;
    let out = out.take();
    if stats {
        eprintln!("vkui: {} builds, {} frames", out.builds, out.frames);
    }
    match out.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// `file://` URIs from a `text/uri-list` drop as local paths. Other schemes
/// (e.g. `https://` from a browser) and other hosts are skipped.
fn uri_paths(uris: &[String]) -> Vec<PathBuf> {
    uris.iter().filter_map(|uri| uri_path(uri.trim())).collect()
}

fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///p` or `file://localhost/p`.
    let path = if rest.starts_with('/') { rest } else { rest.strip_prefix("localhost")? };
    let mut bytes = Vec::with_capacity(path.len());
    let mut it = path.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        // `file:///C:/x` is `C:/x`.
        let path = String::from_utf8(bytes).ok()?;
        let path = match path.as_bytes() {
            [b'/', d, b':', ..] if d.is_ascii_alphabetic() => &path[1..],
            _ => &path[..],
        };
        Some(PathBuf::from(path))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn file_uris() {
        let uris = [
            "file:///home/a/My%20File.txt",
            "file://localhost/tmp/x",
            "file://otherhost/tmp/y",
            "https://example.com/z",
            "file:///bad%2",
            "file:///caf%C3%A9 \r",
        ]
        .map(String::from);
        let paths = uri_paths(&uris);
        assert_eq!(
            paths,
            ["/home/a/My File.txt", "/tmp/x", "/caf\u{e9}"].map(PathBuf::from)
        );
    }
}

impl<A: App> Runner<A> {
    /// Rebuilds the UI; requests a redraw if the output changed.
    fn rebuild(&mut self, force: bool, event_loop: &dyn ActiveEventLoop) {
        let (Some(window), Some(ui)) = (&self.window, &mut self.ui) else {
            return;
        };
        let size = window.surface_size();
        let scale = window.scale_factor() as f32;
        let mut cx = Cx::default();
        // A glyph-atlas reset invalidates the frame; the next pass then
        // finds every glyph in the fresh atlas. A state change during the
        // build also takes one more pass.
        let mut changed = force;
        for _ in 0..4 {
            self.builds += 1;
            cx.again = false;
            ui.begin(size.width as f32 / scale, size.height as f32 / scale, scale);
            self.app.build(ui, &mut cx);
            let Some((quads, batches)) = ui.finish() else {
                continue;
            };
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
        if changed {
            window.request_redraw();
        }
        if ui.cursor != self.cursor {
            self.cursor = ui.cursor;
            window.set_cursor(self.cursor.into());
        }
        if cx.close {
            event_loop.exit();
        }
        if let Some(text) = cx.copy.take() {
            let data = DataTransferSendBuilder::new(text)
                .with_type(TypeHint::Plaintext, |text: &String, _: &dyn TransferType| Some(text.clone()))
                .build();
            if let Err(e) = event_loop.set_clipboard(data) {
                self.clipboard_error(format!("Cannot copy to the clipboard: {e}"), event_loop);
            }
        }
        if cx.paste {
            self.paste(event_loop, false);
        }
    }

    fn clipboard_error(&mut self, error: String, event_loop: &dyn ActiveEventLoop) {
        if self.trace {
            eprintln!("vkui: {error}");
        }
        self.app.clipboard_error(error);
        self.rebuild(false, event_loop);
    }

    /// Reads the clipboard: text into the focused text field (`field`), or
    /// else everything it holds, for [`App::pasted`].
    fn paste(&mut self, event_loop: &dyn ActiveEventLoop, field: bool) {
        let id = match event_loop.clipboard() {
            Ok(id) => id,
            Err(e) => return self.clipboard_error(format!("Cannot read the clipboard: {e}"), event_loop),
        };
        let (files, image, text) = id.and_then(|id| event_loop.data_transfer(id).ok()).map_or(
            (false, None, false),
            |dt| (dt.has_type(&TypeHint::UriList), image_type(&*dt), dt.has_type(&TypeHint::Plaintext)),
        );
        if self.trace {
            eprintln!("vkui: paste field={field} files={files} image={image:?} text={text}");
        }
        if field {
            if let Some(id) = id
                && text
                && let Err(e) = self.fetch(event_loop, id, TypeHint::Plaintext, Fetch::Field)
            {
                self.clipboard_error(format!("Cannot read the clipboard: {e}"), event_loop);
            }
            return;
        }
        // A newer paste replaces one still being read.
        self.fetches.retain(|(_, fetch)| !fetch.is_gather());
        self.gather = Gather::default();
        if let Some(id) = id {
            let mut forms = Vec::new();
            if files {
                forms.push((TypeHint::UriList, Fetch::Files));
            }
            if let Some(extension) = image {
                forms.push((TypeHint::Image { extension_hint: Some(extension) }, Fetch::Image { extension }));
            }
            if text {
                forms.push((TypeHint::Plaintext, Fetch::Text));
            }
            for (hint, fetch) in forms {
                match self.fetch(event_loop, id, hint, fetch) {
                    Ok(()) => self.gather.waiting += 1,
                    Err(e) => self.gather.error = Some(e.to_string()),
                }
            }
        }
        if self.gather.waiting == 0 {
            self.gathered(event_loop);
        }
    }

    /// Hands a paste to the app once every form has been read.
    fn gathered(&mut self, event_loop: &dyn ActiveEventLoop) {
        let gather = std::mem::take(&mut self.gather);
        let mut pasted = Pasted { files: uri_paths(&gather.uris), image: gather.image, text: gather.text };
        pasted.drop_repeats(&gather.uris);
        if self.trace {
            eprintln!(
                "vkui: pasted files={:?} image={:?} text={}",
                pasted.files,
                pasted.image.as_ref().map(|(data, extension)| (data.len(), extension)),
                pasted.text.is_some()
            );
        }
        if let (0, Some(e)) = (pasted.kinds(), gather.error) {
            return self.clipboard_error(format!("Cannot read the clipboard: {e}"), event_loop);
        }
        self.app.pasted(pasted);
        self.rebuild(false, event_loop);
    }

    fn fetch(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        id: DataTransferId,
        hint: TypeHint,
        fetch: Fetch,
    ) -> Result<(), winit::error::TransferError> {
        let serial = event_loop.fetch_data_transfer(id, &hint)?;
        self.fetches.push((serial, fetch));
        Ok(())
    }

    fn received(&mut self, event_loop: &dyn ActiveEventLoop, fetch: Fetch, value: &dyn TypedData) {
        match fetch {
            Fetch::Drop => {
                let uris = value.try_as_uris();
                if self.trace {
                    eprintln!("vkui: dropped {uris:?}");
                }
                for path in uris.map(|uris| uri_paths(&uris)).unwrap_or_default() {
                    self.app.dropped(path);
                }
            }
            Fetch::Field => match value.try_as_string() {
                Ok(text) => {
                    if let Some(ui) = &mut self.ui {
                        ui.paste(&text);
                    }
                }
                Err(e) => return self.clipboard_error(format!("Cannot read the clipboard: {e}"), event_loop),
            },
            Fetch::Files | Fetch::Image { .. } | Fetch::Text => {
                let gather = &mut self.gather;
                let result = match fetch {
                    Fetch::Files => value.try_as_uris().map(|uris| gather.uris = uris),
                    Fetch::Image { extension } => value
                        .try_as_bytes()
                        .map(|data| gather.image = (!data.is_empty()).then_some((data, extension))),
                    _ => value.try_as_string().map(|text| gather.text = (!text.trim().is_empty()).then_some(text)),
                };
                if let Err(e) = result {
                    gather.error = Some(e.to_string());
                }
                gather.waiting -= 1;
                if gather.waiting == 0 {
                    return self.gathered(event_loop);
                }
                return;
            }
        }
        self.rebuild(false, event_loop);
    }

    fn fail(&mut self, error: String, event_loop: &dyn ActiveEventLoop) {
        self.out.borrow_mut().error = Some(error);
        event_loop.exit();
    }

    /// Drops: only files are accepted (`text/uri-list`), and the list is
    /// fetched once, on the drop itself, not while hovering.
    fn drag(&mut self, event_loop: &dyn ActiveEventLoop, id: DataTransferId, dropped: bool) {
        let files = event_loop.data_transfer(id).is_ok_and(|dt| dt.has_type(&TypeHint::UriList));
        if !dropped {
            let actions: &[DndAction] = if files { &[DndAction::Copy] } else { &[] };
            let _ = event_loop.set_valid_dnd_actions(id, actions);
        } else if files {
            let _ = self.fetch(event_loop, id, TypeHint::UriList, Fetch::Drop);
        }
        if self.trace {
            eprintln!("vkui: drag dropped={dropped} files={files}");
        }
    }
}

impl<A: App> Drop for Runner<A> {
    fn drop(&mut self) {
        let mut out = self.out.borrow_mut();
        out.builds = self.builds;
        out.frames = self.frames;
    }
}

impl<A: App> ApplicationHandler for Runner<A> {
    fn new_events(&mut self, _: &dyn ActiveEventLoop, cause: winit::event::StartCause) {
        if self.trace {
            eprintln!("vkui: {:>8} ms wake {cause:?}", self.started.elapsed().as_millis());
        }
    }

    fn device_event(
        &mut self,
        _: &dyn ActiveEventLoop,
        _: Option<winit::event::DeviceId>,
        event: winit::event::DeviceEvent,
    ) {
        if self.trace {
            eprintln!("vkui: device {event:?}");
        }
    }

    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title(self.app.title())
            .with_surface_size(LogicalSize::new(440.0, 700.0))
            .with_min_surface_size(LogicalSize::new(340.0, 420.0));
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "ios"))))]
        let attributes = {
            use winit::platform::{wayland, x11};
            let app_id = self.app.app_id().to_string();
            if wayland::ActiveEventLoopExtWayland::is_wayland(event_loop) {
                attributes.with_platform_attributes(Box::new(
                    wayland::WindowAttributesWayland::default().with_name(&app_id, ""),
                ))
            } else {
                attributes.with_platform_attributes(Box::new(
                    x11::WindowAttributesX11::default().with_name(&app_id, &app_id),
                ))
            }
        };
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(e) => return self.fail(format!("Cannot open a window: {e}"), event_loop),
        };
        // Text input through the IME (compose, CJK); no cursor area is reported.
        let ime = ImeEnableRequest::new(
            ImeCapabilities::new().with_hint_and_purpose(),
            ImeRequestData::default().with_hint_and_purpose(ImeHint::NONE, ImePurpose::Normal),
        );
        if let Some(ime) = ime {
            let _ = window.request_ime_update(ImeRequest::Enable(ime));
        }
        let theme = match window.theme() {
            Some(winit::window::Theme::Light) => Theme::LIGHT,
            _ => Theme::DARK,
        };
        match (Gfx::new(&*window), Ui::new(theme)) {
            (Ok(gfx), Ok(ui)) => {
                if self.stats {
                    eprintln!("vkui: rendering on {}", gfx.device_name);
                }
                self.gfx = Some(gfx);
                self.ui = Some(ui);
            }
            (Err(e), _) | (_, Err(e)) => return self.fail(e, event_loop),
        }
        self.window = Some(window);
        self.rebuild(true, event_loop);
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.app.woken() {
            self.rebuild(false, event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.trace {
            eprintln!("vkui: event {event:?}");
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::SurfaceResized(size) => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.resize(size.width, size.height);
                }
                self.rebuild(true, event_loop);
            }
            WindowEvent::ScaleFactorChanged { .. } => self.rebuild(true, event_loop),
            WindowEvent::ThemeChanged(theme) => {
                if let Some(ui) = &mut self.ui {
                    ui.theme = match theme {
                        winit::window::Theme::Light => Theme::LIGHT,
                        winit::window::Theme::Dark => Theme::DARK,
                    };
                }
                self.rebuild(true, event_loop);
            }
            WindowEvent::DragEntered { id, .. } => self.drag(event_loop, id, false),
            WindowEvent::DragDropped { id, .. } => self.drag(event_loop, id, true),
            WindowEvent::DataTransferReceived { serial, value, .. } => {
                if let Some(i) = self.fetches.iter().position(|&(s, _)| s == serial) {
                    let (_, fetch) = self.fetches.swap_remove(i);
                    self.received(event_loop, fetch, &*value);
                }
            }
            WindowEvent::RedrawRequested => {
                let (Some(gfx), Some(ui)) = (&mut self.gfx, &mut self.ui) else {
                    return;
                };
                let clear = ui.theme.bg.floats();
                match gfx.draw(&self.quads, &self.batches, clear, &mut ui.text) {
                    Ok(true) => {
                        self.frames += 1;
                        if self.stats {
                            eprintln!(
                                "vkui: frame {} ({} builds) at {} ms",
                                self.frames,
                                self.builds,
                                self.started.elapsed().as_millis()
                            );
                        }
                    }
                    // Out of date: the next draw rebuilds the swapchain.
                    Ok(false) => {
                        if let Some(window) = &self.window
                            && window.surface_size().width > 0
                        {
                            window.request_redraw();
                        }
                    }
                    Err(e) => self.fail(e, event_loop),
                }
            }
            event => {
                let Some(ui) = &mut self.ui else { return };
                let changed = ui.event(&event);
                let paste = std::mem::take(&mut ui.paste_key);
                let field = ui.focus.is_some();
                if paste {
                    self.paste(event_loop, field);
                }
                if changed {
                    self.rebuild(false, event_loop);
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod paste_tests {
    use super::*;

    #[test]
    fn pasted_repeats() {
        let dir = std::env::temp_dir().join(format!("vkui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.png");
        std::fs::write(&file, b"PNG").unwrap();
        let uri = format!("file://{}", file.display().to_string().replace(' ', "%20"));
        let paste = |image: &[u8], text: &str| {
            let mut pasted = Pasted {
                files: uri_paths(std::slice::from_ref(&uri)),
                image: Some((image.to_vec(), "png")),
                text: Some(text.to_string()),
            };
            pasted.drop_repeats(std::slice::from_ref(&uri));
            pasted
        };
        // A file manager: the file, its path or URI as text, and its content
        // as an image are one thing.
        let pasted = paste(b"PNG", &format!("{}\n", file.display()));
        assert_eq!((pasted.files.len(), pasted.image.is_some(), pasted.text.is_some()), (1, false, false));
        assert_eq!(paste(b"PNG", &uri).text, None);
        // Different things offered together are all kept.
        let pasted = paste(b"other image", "some words");
        assert_eq!((pasted.files.len(), pasted.image.is_some(), pasted.text.is_some()), (1, true, true));
        assert_eq!(pasted.kinds(), 3);
        // Text that lists the file and more is not a repeat.
        assert!(paste(b"PNG", &format!("{}\nand more", file.display())).text.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
