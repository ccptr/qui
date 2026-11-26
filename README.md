# qui

A small immediate-mode GUI toolkit on ash (Vulkan) and its own Wayland client
(windows and layer-shell overlays), or X11 where there is no Wayland, or Win32 on
Windows, with its own TrueType rasterizer. It is
event-driven: the UI is rebuilt after each input or app event, and a frame is only
rendered when the quads changed.

## Design

* `font.rs`: a TrueType parser and rasterizer (glyf, simple and composite outlines,
  cmap 4/12, TTC), and colour bitmap glyphs (CBDT, as in Noto Color Emoji). Font
  files are memory-mapped. Coverage is computed by signed-area accumulation at four
  sub-pixel offsets. CFF/OTF fonts aren't supported; they are skipped.
* `text.rs`: a glyph cache on a 1024² R8 atlas (shelf packing), plus measuring,
  eliding and wrapping. Characters the first fonts lack fall back through
  fontconfig's list (`fc-match -s`; on Windows, the system fonts for other
  scripts), opened only when needed; emoji prefer the colour emoji font.
* `image.rs`: a 1024² RGBA atlas for images and colour emoji, repacked when full,
  with CPU copies trimmed to a budget.
* `gfx.rs`: Vulkan through `ash`. There's one pipeline: instanced quads, each an SDF
  rounded rectangle (with an optional border), a soft shadow, a glyph or an image.
  Shaders are precompiled SPIR-V. It prefers the integrated GPU, FIFO present, two
  frames in flight, and the driver's minimum swapchain image count. The surface can
  be detached and attached again, so a hidden window keeps the device but no
  swapchain. A layer surface marked `keep_when_hidden` keeps its swapchain too:
  hiding *parks* it (the layer role is dropped, so nothing is mapped). Showing
  gives the role back; the compositor answers with the scale and a configure,
  the frame is drawn at that scale, and the commit after it maps the window, a
  round trip and a frame later (about a millisecond). A parked surface can be
  prepared ahead: it takes the layer role only until the configure, which tells
  the scale, then gets its swapchain without presenting anything. Parked
  surfaces present with MAILBOX, so a present never waits on a frame callback
  from before the surface was hidden.
* `ui.rs`: immediate-mode widgets (buttons, text fields with IME and paste,
  checkboxes, scrolling, clipping), rebuilt on every event.
* `window/`: the windowing. `mod.rs` holds what the backends share: the API
  (`App`, `Spec`, `Cx`, `run`), the frame building and drawing, key translation
  and paste handling. `run` picks a backend.
* `window/wayland.rs`: a native Wayland client on `wayland-client` (the
  `wayland` feature, on by default; libwayland and libxkbcommon are opened at
  run time). One `poll` over the Wayland socket, a
  wake-up eventfd and the key-repeat timerfd. It shows a normal window (with
  server-side decorations through `xdg-decoration` where offered) or a layer-shell
  overlay; fractional scaling, cursor shapes, IME through `text-input-v3`, the
  clipboard and file drops (read in a thread, only on a paste or a drop). Input or
  an app event rebuilds the UI, which takes microseconds for a few hundred quads. A
  frame is rendered only when the quads changed.
* `window/x11.rs`: the same apps on X11 (the `x11` feature, on by default), used when
  there is a `DISPLAY` but no Wayland; `QUI_BACKEND=x11` or `wayland` forces one.
  It is an x11rb client of libxcb (opened at run time), with xkbcommon-x11 for the
  keyboard and `VK_KHR_xcb_surface`, which both need libxcb's connection rather
  than x11rb's pure-Rust one. A layer-shell spec becomes an override-redirect
  window placed by its anchors and margins on the monitor under the pointer; with
  an exclusive keyboard it grabs the keyboard and pointer, so clicks elsewhere are
  clicks outside. The scale is `Xft.dpi` / 96. Hiding a `keep_when_hidden` window
  unmaps it and keeps its swapchain. The clipboard (with INCR for large data),
  XDND file drops and themed cursors work; exclusive zones, blur and input
  methods don't.
* `window/win32.rs`: the same apps on Windows 10 (1703) or later, through Win32
  (`windows-sys`) and `VK_KHR_win32_surface`; the `wayland` and `x11` features
  do nothing there. The window procedures record messages and the loop handles
  them, as on X11; during Windows' modal loops (moving or resizing a window)
  the procedures run the loop's step themselves, so the content follows a resize.
  A layer-shell spec becomes a borderless popup, off the taskbar, placed by its
  anchors and margins in the work area of the monitor under the pointer; with an
  exclusive keyboard it takes the focus, and losing it is a click outside. It is
  per-monitor DPI aware (the scale is the monitor's DPI / 96). Keys are
  translated to XKB keysyms and evdev codes, so bindings match as on Linux; text
  comes from `WM_CHAR` (the input method's too, through its default window). The
  clipboard (text, files, PNG or bitmap images), file drops, cursors and a dark
  title bar under a dark theme work; transparency, exclusive zones and blur don't.

## Building and testing

```sh
cargo test                                   # rasterizer: exact coverage of a square and a half-pixel edge, plus a real system font
cargo run --example layer --features png     # demo as a layer-shell overlay (--window: a normal window)
shaders/build.sh                             # only after editing a shader: regenerates the committed SPIR-V
cargo build --target x86_64-pc-windows-gnu --example layer --features png   # Windows, cross-built with mingw-w64
```

`QUI_STATS=1` prints the GPU name and the time of every presented frame;
`QUI_EVENTS=1` prints every input event; `QUI_EMOJI_FONT` picks the colour emoji font.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in qui by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
