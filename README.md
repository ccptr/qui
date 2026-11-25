# qui

A small immediate-mode GUI toolkit on ash (Vulkan) and its own Wayland client
(windows and layer-shell overlays), with its own TrueType rasterizer. It is
event-driven: the UI is rebuilt after each input or app event, and a frame is only
rendered when the quads changed.

## Design

* `font.rs`: a TrueType parser and rasterizer (glyf, simple and composite outlines,
  cmap 4/12, TTC), and colour bitmap glyphs (CBDT, as in Noto Color Emoji). Font
  files are memory-mapped. Coverage is computed by signed-area accumulation at four
  sub-pixel offsets. CFF/OTF fonts aren't supported; they are skipped.
* `text.rs`: a glyph cache on a 1024² R8 atlas (shelf packing), plus measuring,
  eliding and wrapping. Characters the first fonts lack fall back through
  fontconfig's list (`fc-match -s`), opened only when needed; emoji prefer the
  colour emoji font.
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
* `wl.rs`: the windowing, a native Wayland client on `wayland-client` (libwayland
  and libxkbcommon are opened at run time). One `poll` over the Wayland socket, a
  wake-up eventfd and the key-repeat timerfd. It shows a normal window (with
  server-side decorations through `xdg-decoration` where offered) or a layer-shell
  overlay; fractional scaling, cursor shapes, IME through `text-input-v3`, the
  clipboard and file drops (read in a thread, only on a paste or a drop). Input or
  an app event rebuilds the UI, which takes microseconds for a few hundred quads. A
  frame is rendered only when the quads changed.

## Building and testing

```sh
cargo test                                   # rasterizer: exact coverage of a square and a half-pixel edge, plus a real system font
cargo run --example layer --features png     # demo as a layer-shell overlay
shaders/build.sh                             # only after editing a shader: regenerates the committed SPIR-V
```

`QUI_STATS=1` prints the GPU name and the time of every presented frame;
`QUI_EVENTS=1` prints every input event; `QUI_EMOJI_FONT` picks the colour emoji font.
