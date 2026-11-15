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
use std::path::PathBuf;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Window, WindowId};

/// Requests from the app to the runner during [`App::build`].
#[derive(Default)]
pub struct Cx {
    pub close: bool,
    /// The app changed its state while building (e.g. a click opened a
    /// dialog), so parts drawn earlier are stale: build once more. The
    /// input was consumed by the first pass.
    pub again: bool,
}

pub trait App: 'static {
    /// Events the app sends itself through the [`EventLoopProxy`] (e.g.
    /// "the engine state changed").
    type Event: 'static + Send;

    fn title(&self) -> String;

    /// Wayland app id / X11 class, matched against `.desktop` files.
    fn app_id(&self) -> &str {
        "vkui"
    }

    /// Builds the whole UI for this frame.
    fn build(&mut self, ui: &mut Ui, cx: &mut Cx);

    /// An app event arrived; return whether the UI must be rebuilt.
    fn event(&mut self, event: Self::Event) -> bool;

    /// A file was dropped onto the window.
    fn dropped(&mut self, _path: PathBuf) {}
}

struct Runner<A: App> {
    app: A,
    window: Option<Arc<Window>>,
    gfx: Option<Gfx>,
    ui: Option<Ui>,
    /// The last built frame, drawn on redraw.
    quads: Vec<Quad>,
    batches: Vec<Batch>,
    cursor: CursorIcon,
    error: Option<String>,
    frames: u64,
    builds: u64,
    started: std::time::Instant,
    stats: bool,
    /// RLS_EVENTS: log every wakeup and event (to find idle wakeups).
    trace: bool,
}

/// Opens the window and runs until it is closed. `make` gets a proxy for
/// sending [`App::Event`]s from other threads.
pub fn run<A: App>(make: impl FnOnce(EventLoopProxy<A::Event>) -> A) -> Result<(), String> {
    // RLS_STATS times frames from here, so `make` (the app's own start-up)
    // counts too.
    let started = std::time::Instant::now();
    let event_loop = EventLoop::<A::Event>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let app = make(event_loop.create_proxy());
    let mut runner = Runner {
        app,
        window: None,
        gfx: None,
        ui: None,
        quads: Vec::new(),
        batches: Vec::new(),
        cursor: CursorIcon::Default,
        error: None,
        frames: 0,
        builds: 0,
        started,
        stats: std::env::var_os("RLS_STATS").is_some(),
        trace: std::env::var_os("RLS_EVENTS").is_some(),
    };
    event_loop.run_app(&mut runner).map_err(|e| e.to_string())?;
    if runner.stats {
        eprintln!("vkui: {} builds, {} frames", runner.builds, runner.frames);
    }
    match runner.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

impl<A: App> Runner<A> {
    /// Rebuilds the UI; requests a redraw if the output changed.
    fn rebuild(&mut self, force: bool, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(ui)) = (&self.window, &mut self.ui) else {
            return;
        };
        let size = window.inner_size();
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
            window.set_cursor(self.cursor);
        }
        if cx.close {
            event_loop.exit();
        }
    }

    fn fail(&mut self, error: String, event_loop: &ActiveEventLoop) {
        self.error = Some(error);
        event_loop.exit();
    }
}

impl<A: App> ApplicationHandler<A::Event> for Runner<A> {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: winit::event::StartCause) {
        if self.trace {
            eprintln!("vkui: {:>8} ms wake {cause:?}", self.started.elapsed().as_millis());
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: winit::event::DeviceId, event: winit::event::DeviceEvent) {
        if self.trace {
            eprintln!("vkui: device {event:?}");
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.app.title())
            .with_inner_size(LogicalSize::new(440.0, 700.0))
            .with_min_inner_size(LogicalSize::new(340.0, 420.0));
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "ios"))))]
        let attributes = {
            let app_id = self.app.app_id().to_string();
            let attributes = winit::platform::wayland::WindowAttributesExtWayland::with_name(
                attributes, &app_id, "",
            );
            winit::platform::x11::WindowAttributesExtX11::with_name(attributes, &app_id, &app_id)
        };
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => return self.fail(format!("Cannot open a window: {e}"), event_loop),
        };
        window.set_ime_allowed(true);
        let theme = match window.theme() {
            Some(winit::window::Theme::Light) => Theme::LIGHT,
            _ => Theme::DARK,
        };
        match (Gfx::new(&window), Ui::new(theme)) {
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: A::Event) {
        if self.app.event(event) {
            self.rebuild(false, event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.trace {
            eprintln!("vkui: event {event:?}");
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
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
            WindowEvent::DroppedFile(path) => {
                self.app.dropped(path);
                self.rebuild(false, event_loop);
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
                            && window.inner_size().width > 0
                        {
                            window.request_redraw();
                        }
                    }
                    Err(e) => self.fail(e, event_loop),
                }
            }
            event => {
                if self.ui.as_mut().is_some_and(|ui| ui.event(&event)) {
                    self.rebuild(false, event_loop);
                }
            }
        }
    }
}
