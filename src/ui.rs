//! An immediate-mode widget layer. The application rebuilds its whole UI
//! after every input or state change (a few hundred quads, microseconds);
//! [`Ui::finish`] then yields the quads, and the window only redraws when
//! they differ from the last frame.
//!
//! Coordinates are logical pixels; quads are emitted in physical pixels,
//! snapped to the pixel grid.

use crate::gfx::{Batch, Quad};
use crate::text::{ATLAS, Text};
use std::collections::HashMap;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::cursor::CursorIcon;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    /// Splits off `w` pixels from the right, returning (rest, right part).
    pub fn split_right(&self, w: f32, gap: f32) -> (Rect, Rect) {
        let right = Rect::new(self.right() - w, self.y, w, self.h);
        (Rect::new(self.x, self.y, (self.w - w - gap).max(0.0), self.h), right)
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        Rect::new(x, y, (self.right().min(o.right()) - x).max(0.0), (self.bottom().min(o.bottom()) - y).max(0.0))
    }
}

/// `0xRRGGBB` with alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub u32, pub u8);

impl Color {
    pub const fn rgb(hex: u32) -> Color {
        Color(hex, 255)
    }
    pub const fn alpha(self, a: u8) -> Color {
        Color(self.0, a)
    }
    fn packed(self) -> u32 {
        let r = (self.0 >> 16) & 0xFF;
        let g = (self.0 >> 8) & 0xFF;
        let b = self.0 & 0xFF;
        r | (g << 8) | (b << 16) | ((self.1 as u32) << 24)
    }
    pub fn floats(self) -> [f32; 4] {
        [
            ((self.0 >> 16) & 0xFF) as f32 / 255.0,
            ((self.0 >> 8) & 0xFF) as f32 / 255.0,
            (self.0 & 0xFF) as f32 / 255.0,
            self.1 as f32 / 255.0,
        ]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub hover: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub danger: Color,
    pub ok: Color,
    pub scrim: Color,
}

impl Theme {
    pub const DARK: Theme = Theme {
        bg: Color::rgb(0x121416),
        surface: Color::rgb(0x1d2023),
        hover: Color::rgb(0x272b2f),
        border: Color::rgb(0x33383d),
        text: Color::rgb(0xe6e8ea),
        muted: Color::rgb(0x9aa1a8),
        accent: Color::rgb(0x26a69a),
        on_accent: Color::rgb(0x06201d),
        danger: Color::rgb(0xef5350),
        ok: Color::rgb(0x66bb6a),
        scrim: Color(0x000000, 150),
    };
    pub const LIGHT: Theme = Theme {
        bg: Color::rgb(0xf3f5f6),
        surface: Color::rgb(0xffffff),
        hover: Color::rgb(0xe9edef),
        border: Color::rgb(0xd5dadd),
        text: Color::rgb(0x1b1f22),
        muted: Color::rgb(0x5f6870),
        accent: Color::rgb(0x00897b),
        on_accent: Color::rgb(0xffffff),
        danger: Color::rgb(0xd32f2f),
        ok: Color::rgb(0x2e7d32),
        scrim: Color(0x000000, 100),
    };
}

pub type Id = u64;

/// A stable widget id from a label and an index (FNV-1a).
pub fn id(label: &str, index: u64) -> Id {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in label.bytes().chain(index.to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ButtonStyle {
    Primary,
    Plain,
    Danger,
    Ghost,
}

#[derive(Default, Clone, Copy)]
pub struct Response {
    pub hovered: bool,
    pub clicked: bool,
}

/// What a text field reports.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    None,
    Changed,
    /// Enter (single line) or Ctrl+Enter (multi-line).
    Submit,
}

#[derive(Clone, Copy, PartialEq)]
enum KeyPress {
    Back,
    Delete,
    Left,
    Right,
    Home,
    End,
    Enter,
    Escape,
    Tab,
}

pub struct Ui {
    pub text: Text,
    pub theme: Theme,
    pub scale: f32,
    /// Window size in logical pixels.
    pub width: f32,
    pub height: f32,

    quads: Vec<Quad>,
    batches: Vec<Batch>,
    clips: Vec<Rect>,

    mouse: (f32, f32),
    mouse_down: bool,
    press_at: Option<(f32, f32)>,
    pressed: bool,
    released: bool,
    wheel: f32,
    modifiers: ModifiersState,
    keys: Vec<KeyPress>,
    typed: String,
    /// Clipboard text for the focused field, delivered by [`Ui::paste`].
    pasted: String,
    /// Ctrl+V (or Shift+Insert) was pressed; the runner reads the clipboard.
    pub(crate) paste_key: bool,
    /// Widget the mouse went down on.
    active: Option<Id>,
    pub focus: Option<Id>,
    focus_seen: bool,
    /// Input is ignored while building layers below a modal.
    pub blocked: bool,
    pub cursor: CursorIcon,
    /// Per scroll area: (offset, largest offset as of the last frame).
    scroll: HashMap<Id, (f32, f32)>,
    cursors: HashMap<Id, usize>,
    /// The pointer left the window.
    outside: bool,
}

impl Ui {
    pub fn new(theme: Theme) -> Result<Ui, String> {
        Ok(Ui {
            text: Text::new()?,
            theme,
            scale: 1.0,
            width: 1.0,
            height: 1.0,
            quads: Vec::new(),
            batches: Vec::new(),
            clips: Vec::new(),
            mouse: (-1.0, -1.0),
            mouse_down: false,
            press_at: None,
            pressed: false,
            released: false,
            wheel: 0.0,
            modifiers: ModifiersState::empty(),
            keys: Vec::new(),
            typed: String::new(),
            pasted: String::new(),
            paste_key: false,
            active: None,
            focus: None,
            focus_seen: false,
            blocked: false,
            cursor: CursorIcon::Default,
            scroll: HashMap::new(),
            cursors: HashMap::new(),
            outside: true,
        })
    }

    /// Feeds a window event. Returns whether the UI must be rebuilt.
    pub fn event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::PointerMoved { position, primary: true, .. } => {
                self.mouse = (position.x as f32 / self.scale, position.y as f32 / self.scale);
                self.outside = false;
                true
            }
            WindowEvent::PointerLeft { primary: true, .. } => {
                self.outside = true;
                self.mouse = (-1.0, -1.0);
                true
            }
            WindowEvent::PointerButton { state, button, primary: true, .. }
                if button.clone().mouse_button() == Some(MouseButton::Left) =>
            {
                match state {
                    ElementState::Pressed => {
                        self.mouse_down = true;
                        self.pressed = true;
                        self.press_at = Some(self.mouse);
                    }
                    ElementState::Released => {
                        self.mouse_down = false;
                        self.released = true;
                    }
                }
                true
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 48.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / self.scale,
                    _ => return false,
                };
                true
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                false
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.typed.push_str(text);
                true
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if is_paste(event, self.ctrl(), self.modifiers.shift_key()) {
                    self.paste_key = true;
                    return false;
                }
                let key = match &event.logical_key {
                    Key::Named(NamedKey::Backspace) => Some(KeyPress::Back),
                    Key::Named(NamedKey::Delete) => Some(KeyPress::Delete),
                    Key::Named(NamedKey::ArrowLeft) => Some(KeyPress::Left),
                    Key::Named(NamedKey::ArrowRight) => Some(KeyPress::Right),
                    Key::Named(NamedKey::Home) => Some(KeyPress::Home),
                    Key::Named(NamedKey::End) => Some(KeyPress::End),
                    Key::Named(NamedKey::Enter) => Some(KeyPress::Enter),
                    Key::Named(NamedKey::Escape) => Some(KeyPress::Escape),
                    Key::Named(NamedKey::Tab) => Some(KeyPress::Tab),
                    _ => None,
                };
                if let Some(key) = key {
                    self.keys.push(key);
                } else if let Some(text) = &event.text
                    && !self.modifiers.control_key()
                    && !self.modifiers.meta_key()
                {
                    self.typed.extend(text.chars().filter(|c| !c.is_control()));
                }
                true
            }
            _ => false,
        }
    }

    /// Starts a frame.
    pub fn begin(&mut self, width: f32, height: f32, scale: f32) {
        self.width = width;
        self.height = height;
        self.scale = scale;
        self.quads.clear();
        self.batches.clear();
        self.clips.clear();
        self.cursor = CursorIcon::Default;
        self.blocked = false;
        self.focus_seen = false;
        self.text.reset = false;
        self.batches.push(Batch {
            clip: [0, 0, i32::MAX, i32::MAX],
            first: 0,
            count: 0,
        });
    }

    /// Ends the frame: consumes the input of this frame and returns the
    /// quads. `None` means the glyph atlas was reset mid-frame and the
    /// frame must be built again.
    pub fn finish(&mut self) -> Option<(&[Quad], &[Batch])> {
        if self.text.reset {
            return None;
        }
        if self.released {
            self.active = None;
        }
        self.pressed = false;
        self.released = false;
        self.wheel = 0.0;
        self.keys.clear();
        self.typed.clear();
        self.pasted.clear();
        if !self.focus_seen {
            self.focus = None;
        }
        Some((&self.quads, &self.batches))
    }

    /// Whether this frame had a key press of Escape (for closing dialogs).
    pub fn escape(&self) -> bool {
        !self.blocked && self.keys.contains(&KeyPress::Escape)
    }

    pub fn ctrl(&self) -> bool {
        self.modifiers.control_key() || self.modifiers.meta_key()
    }

    /// Clipboard text for the focused text field, inserted on the next build.
    pub(crate) fn paste(&mut self, text: &str) {
        self.pasted = text
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c == '\t' || c == '\r' { ' ' } else { c })
            .filter(|&c| c == '\n' || !c.is_control())
            .collect();
    }

    // ---- drawing ------------------------------------------------------------

    fn snap(&self, v: f32) -> f32 {
        (v * self.scale).round()
    }

    fn physical(&self, r: Rect) -> [f32; 4] {
        let x0 = self.snap(r.x);
        let y0 = self.snap(r.y);
        [x0, y0, self.snap(r.right()) - x0, self.snap(r.bottom()) - y0]
    }

    fn push(&mut self, quad: Quad) {
        self.quads.push(quad);
        self.batches.last_mut().unwrap().count += 1;
    }

    pub fn rect(&mut self, r: Rect, color: Color, radius: f32) {
        if color.1 == 0 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        self.push(Quad {
            rect: self.physical(r),
            uv: [0.0; 4],
            color: color.packed(),
            params: [radius * self.scale, 0.0, 0.0],
        });
    }

    pub fn outline(&mut self, r: Rect, color: Color, radius: f32, width: f32) {
        self.push(Quad {
            rect: self.physical(r),
            uv: [0.0; 4],
            color: color.packed(),
            params: [radius * self.scale, (width * self.scale).max(1.0), 0.0],
        });
    }

    /// Restricts drawing to `r` until [`Ui::pop_clip`].
    pub fn push_clip(&mut self, r: Rect) {
        let r = match self.clips.last() {
            Some(outer) => r.intersect(outer),
            None => r,
        };
        self.clips.push(r);
        self.set_batch(Some(r));
    }

    pub fn pop_clip(&mut self) {
        self.clips.pop();
        let r = self.clips.last().copied();
        self.set_batch(r);
    }

    fn set_batch(&mut self, r: Option<Rect>) {
        let clip = match r {
            Some(r) => {
                let p = self.physical(r);
                [p[0] as i32, p[1] as i32, (p[0] + p[2]) as i32, (p[1] + p[3]) as i32]
            }
            None => [0, 0, i32::MAX, i32::MAX],
        };
        let first = self.quads.len() as u32;
        let last = self.batches.last_mut().unwrap();
        if last.count == 0 {
            last.clip = clip;
            last.first = first;
        } else {
            self.batches.push(Batch { clip, first, count: 0 });
        }
    }

    /// Draws `s` with its baseline at `ascent` below `y`. Returns the width.
    pub fn label(&mut self, x: f32, y: f32, s: &str, size: f32, color: Color) -> f32 {
        let px = size * self.scale;
        let baseline = self.snap(y) + self.text.ascent(px);
        let start = x * self.scale;
        let mut pen = start;
        let packed = color.packed();
        let inv = 1.0 / ATLAS as f32;
        for c in s.chars() {
            let advance = self.text.advance(c, px);
            let whole = pen.floor();
            if let Some(slot) = self.text.slot(c, px, pen - whole) {
                let gx = whole + slot.left as f32;
                let gy = baseline - slot.top as f32;
                self.push(Quad {
                    rect: [gx, gy, slot.w as f32, slot.h as f32],
                    uv: [
                        slot.x as f32 * inv,
                        slot.y as f32 * inv,
                        (slot.x + slot.w) as f32 * inv,
                        (slot.y + slot.h) as f32 * inv,
                    ],
                    color: packed,
                    params: [0.0, 0.0, 1.0],
                });
            }
            pen += advance;
        }
        (pen - start) / self.scale
    }

    pub fn measure(&mut self, s: &str, size: f32) -> f32 {
        self.text.measure(s, size * self.scale) / self.scale
    }

    pub fn line_height(&self, size: f32) -> f32 {
        self.text.line_height(size * self.scale) / self.scale
    }

    /// One line of text in `r`, vertically centered, elided to fit.
    pub fn text_in(&mut self, r: Rect, s: &str, size: f32, color: Color, align: Align) {
        let px = size * self.scale;
        let s = self.text.elide(s, px, r.w * self.scale);
        let w = self.text.measure(&s, px) / self.scale;
        let x = match align {
            Align::Left => r.x,
            Align::Center => r.x + (r.w - w) * 0.5,
            Align::Right => r.right() - w,
        };
        let lh = self.line_height(size);
        self.label(x, r.y + (r.h - lh) * 0.5, &s, size, color);
    }

    /// Wrapped text starting at `(x, y)`; returns the height used.
    pub fn paragraph(&mut self, x: f32, y: f32, w: f32, s: &str, size: f32, color: Color) -> f32 {
        let px = size * self.scale;
        let lh = self.line_height(size);
        let lines = self.text.wrap(s, px, w * self.scale);
        for (i, &(a, b)) in lines.iter().enumerate() {
            self.label(x, y + i as f32 * lh, &s[a..b], size, color);
        }
        lines.len() as f32 * lh
    }

    pub fn wrapped_height(&mut self, w: f32, s: &str, size: f32) -> f32 {
        let lines = self.text.wrap(s, size * self.scale, w * self.scale).len();
        lines as f32 * self.line_height(size)
    }

    // ---- interaction ----------------------------------------------------------

    fn visible(&self, r: &Rect) -> bool {
        match self.clips.last() {
            Some(clip) => clip.contains(self.mouse) && r.contains(self.mouse),
            None => r.contains(self.mouse),
        }
    }

    /// Hover and click tracking for an area.
    pub fn interact(&mut self, id: Id, r: Rect) -> Response {
        if self.blocked || self.outside {
            return Response::default();
        }
        let hovered = self.visible(&r);
        if hovered && self.pressed {
            self.active = Some(id);
        }
        // A press and release within one frame still counts as a click.
        let pressed_here = self.press_at.is_some_and(|p| {
            r.contains(p) && self.clips.last().is_none_or(|clip| clip.contains(p))
        });
        let clicked = self.released
            && hovered
            && (self.active == Some(id) || (self.pressed && pressed_here));
        if hovered {
            self.cursor = CursorIcon::Pointer;
        }
        Response { hovered, clicked }
    }

    /// Whether the mouse went down outside every widget this frame (to
    /// close a dialog by clicking the scrim).
    pub fn pressed_outside(&self, r: Rect) -> bool {
        !self.blocked && self.pressed && !r.contains(self.mouse)
    }

    pub fn button(&mut self, id: Id, r: Rect, label: &str, style: ButtonStyle) -> bool {
        let res = self.interact(id, r);
        let t = self.theme;
        let pressed = res.hovered && self.mouse_down && self.active == Some(id);
        let (bg, fg) = match style {
            ButtonStyle::Primary => (t.accent, t.on_accent),
            ButtonStyle::Danger => (t.danger.alpha(if res.hovered { 60 } else { 36 }), t.danger),
            ButtonStyle::Plain => (if res.hovered { t.hover } else { t.surface }, t.text),
            ButtonStyle::Ghost => (if res.hovered { t.hover } else { t.bg.alpha(0) }, t.accent),
        };
        let radius = r.h * 0.5;
        self.rect(r, bg, radius);
        if style == ButtonStyle::Primary && (res.hovered || pressed) {
            self.rect(r, Color(0xffffff, if pressed { 50 } else { 28 }), radius);
        }
        if style == ButtonStyle::Plain {
            self.outline(r, t.border, radius, 1.0);
        }
        self.text_in(r.inset((r.w * 0.15).min(10.0), 0.0), label, 14.0, fg, Align::Center);
        res.clicked
    }

    pub fn progress(&mut self, r: Rect, fraction: f32, color: Color) {
        let t = self.theme;
        self.rect(r, t.hover, r.h * 0.5);
        let w = r.w * fraction.clamp(0.0, 1.0);
        if w > 0.0 {
            self.rect(Rect::new(r.x, r.y, w.max(r.h), r.h), color, r.h * 0.5);
        }
    }

    pub fn checkbox(&mut self, id: Id, r: Rect, checked: bool, label: &str) -> bool {
        let res = self.interact(id, r);
        let t = self.theme;
        let box_r = Rect::new(r.x, r.y + (r.h - 18.0) * 0.5, 18.0, 18.0);
        if checked {
            self.rect(box_r, t.accent, 4.0);
            // A check mark from two bars would need rotation; a dot reads fine.
            self.rect(box_r.inset(5.0, 5.0), t.on_accent, 2.0);
        } else {
            self.outline(box_r, if res.hovered { t.accent } else { t.muted }, 4.0, 1.5);
        }
        let text_r = Rect::new(box_r.right() + 10.0, r.y, r.w - 28.0, r.h);
        self.text_in(text_r, label, 14.0, t.text, Align::Left);
        res.clicked
    }

    // ---- scrolling ------------------------------------------------------------

    /// Starts a scrollable area: clips to `r` and returns the content's
    /// y offset. Call [`Ui::end_scroll`] with the content height.
    pub fn begin_scroll(&mut self, id: Id, r: Rect) -> f32 {
        let (mut offset, max) = self.scroll.get(&id).copied().unwrap_or((0.0, 0.0));
        if !self.blocked && r.contains(self.mouse) && self.wheel != 0.0 {
            // Clamped now, against last frame's content height: clamping
            // only in end_scroll would draw this frame past the end, and
            // nothing redraws until the next input moves it back.
            offset = (offset - self.wheel).clamp(0.0, max);
            self.wheel = 0.0;
        }
        self.scroll.insert(id, (offset, max));
        self.push_clip(r);
        r.y - offset.max(0.0)
    }

    pub fn end_scroll(&mut self, id: Id, r: Rect, content: f32) {
        self.pop_clip();
        let max = (content - r.h).max(0.0);
        let offset = self.scroll.get(&id).map_or(0.0, |s| s.0).clamp(0.0, max);
        self.scroll.insert(id, (offset, max));
        if max > 0.0 {
            // Scroll indicator.
            let t = self.theme;
            let bar = (r.h * r.h / content).max(24.0);
            let y = r.y + (r.h - bar) * (offset / max);
            self.rect(Rect::new(r.right() - 4.0, y, 3.0, bar), t.muted.alpha(110), 1.5);
        }
    }

    pub fn reset_scroll(&mut self, id: Id) {
        self.scroll.remove(&id);
    }

    // ---- text editing -----------------------------------------------------------

    /// A text field. Multi-line fields wrap and grow up to `r.h`; Enter
    /// inserts a newline there and Ctrl+Enter submits.
    pub fn text_edit(
        &mut self,
        id: Id,
        r: Rect,
        value: &mut String,
        placeholder: &str,
        multiline: bool,
    ) -> Edit {
        let t = self.theme;
        let res = self.interact(id, r);
        if res.hovered {
            self.cursor = CursorIcon::Text;
        }
        if res.clicked || (self.pressed && res.hovered) {
            self.focus = Some(id);
        }
        let focused = self.focus == Some(id);
        if focused {
            self.focus_seen = true;
        }
        let mut cursor = self
            .cursors
            .get(&id)
            .copied()
            .unwrap_or(value.len())
            .min(value.len());
        while !value.is_char_boundary(cursor) {
            cursor -= 1;
        }

        let mut edit = Edit::None;
        if focused && !self.blocked {
            // A single-line field takes a pasted line without its line
            // break, and several lines joined by spaces.
            let pasted = if multiline { self.pasted.clone() } else { self.pasted.trim().replace('\n', " ") };
            for s in [&self.typed, &pasted] {
                if !s.is_empty() {
                    value.insert_str(cursor, s);
                    cursor += s.len();
                    edit = Edit::Changed;
                }
            }
            let prev = |s: &str, i: usize| s[..i].char_indices().last().map_or(0, |(j, _)| j);
            let next = |s: &str, i: usize| s[i..].chars().next().map_or(i, |c| i + c.len_utf8());
            for key in self.keys.clone() {
                match key {
                    KeyPress::Back if cursor > 0 => {
                        let p = prev(value, cursor);
                        value.replace_range(p..cursor, "");
                        cursor = p;
                        edit = Edit::Changed;
                    }
                    KeyPress::Delete if cursor < value.len() => {
                        let n = next(value, cursor);
                        value.replace_range(cursor..n, "");
                        edit = Edit::Changed;
                    }
                    KeyPress::Left => cursor = prev(value, cursor),
                    KeyPress::Right => cursor = next(value, cursor),
                    KeyPress::Home => cursor = if multiline { value[..cursor].rfind('\n').map_or(0, |i| i + 1) } else { 0 },
                    KeyPress::End => cursor = if multiline { value[cursor..].find('\n').map_or(value.len(), |i| cursor + i) } else { value.len() },
                    KeyPress::Enter if multiline && !self.ctrl() => {
                        value.insert(cursor, '\n');
                        cursor += 1;
                        edit = Edit::Changed;
                    }
                    KeyPress::Enter => edit = Edit::Submit,
                    KeyPress::Escape => self.focus = None,
                    _ => {}
                }
            }
        }
        self.cursors.insert(id, cursor);

        self.rect(r, t.surface, 8.0);
        self.outline(r, if focused { t.accent } else { t.border }, 8.0, if focused { 1.5 } else { 1.0 });
        let inner = r.inset(12.0, if multiline { 10.0 } else { 0.0 });
        self.push_clip(r.inset(2.0, 2.0));
        let size = 14.0;
        let lh = self.line_height(size);
        if value.is_empty() {
            let pr = if multiline { Rect::new(inner.x, inner.y, inner.w, lh) } else { inner };
            self.text_in(pr, placeholder, size, t.muted, Align::Left);
        }
        if multiline {
            let px = size * self.scale;
            let lines = self.text.wrap(value, px, inner.w * self.scale);
            // Keep the caret's line visible.
            let caret_line = lines
                .iter()
                .rposition(|&(a, _)| a <= cursor)
                .unwrap_or(0);
            let visible = ((inner.h / lh).floor() as usize).max(1);
            let first = (caret_line + 1).saturating_sub(visible);
            for (row, &(a, b)) in lines.iter().enumerate().skip(first).take(visible) {
                let y = inner.y + (row - first) as f32 * lh;
                self.label(inner.x, y, &value[a..b], size, t.text);
                if focused && row == caret_line {
                    let x = inner.x + self.measure(&value[a..cursor.min(b).max(a)], size);
                    self.rect(Rect::new(x, y + 2.0, 1.5, lh - 4.0), t.accent, 0.0);
                }
            }
        } else {
            let before = self.measure(&value[..cursor], size);
            let shift = (before - inner.w + 4.0).max(0.0);
            let y = inner.y + (inner.h - lh) * 0.5;
            self.label(inner.x - shift, y, value, size, t.text);
            if focused {
                self.rect(Rect::new(inner.x - shift + before, y + 2.0, 1.5, lh - 4.0), t.accent, 0.0);
            }
        }
        self.pop_clip();
        edit
    }
}

/// Paste shortcuts: Ctrl+V (also Cmd+V and Ctrl+Shift+V), Shift+Insert and
/// the Paste key. V is matched on the layout's own letter, or where V is on
/// a US keyboard for layouts without Latin letters.
fn is_paste(event: &KeyEvent, ctrl: bool, shift: bool) -> bool {
    match &event.logical_key {
        Key::Named(NamedKey::Paste) => return true,
        Key::Named(NamedKey::Insert) => return shift && !ctrl,
        _ => {}
    }
    ctrl && match &event.key_without_modifiers {
        Key::Character(c) if c.is_ascii() => c.eq_ignore_ascii_case("v"),
        _ => event.physical_key == PhysicalKey::Code(KeyCode::KeyV),
    }
}
