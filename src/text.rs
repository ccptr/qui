//! Text: font fallback, a glyph cache backed by a single-channel atlas, and
//! measuring/eliding/wrapping. Glyphs are rasterized once per (font, glyph,
//! size, quarter-pixel offset) and then only composited.

use crate::font::Font;
use crate::image::{ImageId, Images};
use std::collections::HashMap;
use std::path::PathBuf;

/// Atlas side length. 1024² bytes hold well over a thousand UI glyphs.
pub const ATLAS: u32 = 1024;
/// Subpixel positions per pixel.
const BINS: f32 = 4.0;

#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub left: i16,
    pub top: i16,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct Key {
    font: u8,
    glyph: u16,
    px: u16,
    bin: u8,
}

/// Font index of characters drawn as nothing (joiners, variation selectors).
const INVISIBLE: u8 = u8::MAX;
/// Most fonts kept open (primary, fallbacks, emoji).
const MAX_FONTS: usize = 128;

/// A colour glyph in the image atlas, placed like a [`Slot`].
#[derive(Clone, Copy, Debug)]
pub struct ColorSlot {
    pub id: ImageId,
    pub w: u32,
    pub h: u32,
    pub left: f32,
    pub top: f32,
}

pub struct Text {
    fonts: Vec<Font>,
    /// Fallback font files not opened yet (`None`: not listed yet).
    fallback: Option<Vec<PathBuf>>,
    /// The colour emoji font (`None`: not looked for yet; `Some(None)`: none).
    emoji: Option<Option<u8>>,
    map: HashMap<char, (u8, u16)>,
    cache: HashMap<Key, Option<Slot>>,
    pub pixels: Vec<u8>,
    /// Bounding box of atlas texels changed since the last upload.
    pub dirty: Option<[u32; 4]>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// Set when the atlas filled up and was cleared: glyph slots handed out
    /// earlier this frame are stale, so the frame must be built again.
    pub reset: bool,
}

/// Candidate font files, most preferred first. `RLS_FONT` overrides.
pub fn default_font_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("RLS_FONT") {
        paths.push(path);
    }
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        for name in ["segoeui.ttf", "arial.ttf", "tahoma.ttf"] {
            paths.push(format!("{windir}\\Fonts\\{name}"));
        }
    }
    #[cfg(target_os = "macos")]
    paths.extend(
        [
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial Unicode.ttf",
        ]
        .map(String::from),
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    paths.extend(
        [
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
        ]
        .map(String::from),
    );
    paths
}

impl Text {
    /// Loads the first usable font as primary and up to two more as
    /// fallbacks for characters the primary lacks.
    pub fn new() -> Result<Text, String> {
        let mut fonts = Vec::new();
        for path in default_font_paths() {
            if fonts.len() == 3 {
                break;
            }
            if let Some(font) = std::fs::read(&path).ok().and_then(Font::parse) {
                fonts.push(font);
            }
        }
        if fonts.is_empty() {
            return Err(
                "No usable TrueType font found; set RLS_FONT to a .ttf file".to_string(),
            );
        }
        Ok(Text {
            fonts,
            fallback: None,
            emoji: None,
            map: HashMap::new(),
            cache: HashMap::new(),
            pixels: vec![0; (ATLAS * ATLAS) as usize],
            dirty: None,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            reset: false,
        })
    }

    fn lookup(&mut self, c: char) -> (u8, u16) {
        if let Some(&hit) = self.map.get(&c) {
            return hit;
        }
        let found = if is_invisible(c) {
            (INVISIBLE, 0)
        } else {
            self.find(c).unwrap_or((0, 0)) // Glyph 0: the primary font's "missing" box.
        };
        self.map.insert(c, found);
        found
    }

    fn find(&mut self, c: char) -> Option<(u8, u16)> {
        if is_emoji(c)
            && let Some(i) = self.emoji_font()
            && let Some(g) = self.fonts[i as usize].glyph(c)
        {
            return Some((i, g));
        }
        if let Some(hit) = self.fonts.iter().enumerate().find_map(|(i, font)| font.glyph(c).map(|g| (i as u8, g))) {
            return Some(hit);
        }
        // Open more fallback fonts until one has the character.
        let list = self.fallback.get_or_insert_with(fallback_fonts);
        while self.fonts.len() < MAX_FONTS {
            let Some(path) = list.pop() else { break };
            let Some(font) = Font::open(&path) else { continue };
            let glyph = font.glyph(c);
            self.fonts.push(font);
            if let Some(g) = glyph {
                return Some(((self.fonts.len() - 1) as u8, g));
            }
        }
        None
    }

    fn emoji_font(&mut self) -> Option<u8> {
        if let Some(found) = self.emoji {
            return found;
        }
        let found = emoji_font_path().and_then(|path| Font::open(&path)).and_then(|font| {
            (self.fonts.len() < MAX_FONTS).then(|| {
                self.fonts.push(font);
                (self.fonts.len() - 1) as u8
            })
        });
        self.emoji = Some(found);
        found
    }

    /// The colour image for `c` at `px`, if its font has colour glyphs.
    /// Decoded into `images` on first use.
    pub fn color(&mut self, c: char, px: f32, images: &mut Images) -> Option<ColorSlot> {
        let (font, glyph) = self.lookup(c);
        if font == INVISIBLE || !self.fonts[font as usize].is_color() {
            return None;
        }
        let f = &self.fonts[font as usize];
        let bitmap = f.color_bitmap(glyph, px)?;
        let scale = px / bitmap.ppem;
        let w = ((bitmap.width as f32 * scale).round() as u32).max(1);
        let h = ((bitmap.height as f32 * scale).round() as u32).max(1);
        let id = color_id(font, glyph, w);
        if !images.contains(id) {
            let (bw, bh, rgba) = crate::image::decode_png(bitmap.png)?;
            images.insert(id, w, h, crate::image::resize(&rgba, bw, bh, w, h));
        }
        Some(ColorSlot { id, w, h, left: bitmap.bearing_x * scale, top: bitmap.bearing_y * scale })
    }

    pub fn ascent(&self, px: f32) -> f32 {
        (self.fonts[0].ascender * px).round()
    }

    pub fn line_height(&self, px: f32) -> f32 {
        let f = &self.fonts[0];
        ((f.ascender - f.descender + f.line_gap) * px).ceil()
    }

    pub fn advance(&mut self, c: char, px: f32) -> f32 {
        let (font, glyph) = self.lookup(c);
        if font == INVISIBLE {
            return 0.0;
        }
        self.fonts[font as usize].advance(glyph) * px
    }

    pub fn measure(&mut self, text: &str, px: f32) -> f32 {
        text.chars().map(|c| self.advance(c, px)).sum()
    }

    /// The glyph's atlas slot, rasterizing it on first use. `None` for
    /// glyphs without ink (spaces).
    pub fn slot(&mut self, c: char, px: f32, x_frac: f32) -> Option<Slot> {
        let (font, glyph) = self.lookup(c);
        if font == INVISIBLE {
            return None;
        }
        let bin = ((x_frac * BINS) as u8).min(BINS as u8 - 1);
        let key = Key {
            font,
            glyph,
            px: (px * 4.0).round() as u16,
            bin,
        };
        if let Some(slot) = self.cache.get(&key) {
            return *slot;
        }
        let bitmap =
            self.fonts[font as usize].rasterize(glyph, key.px as f32 / 4.0, bin as f32 / BINS);
        let slot = bitmap.and_then(|bitmap| {
            let (x, y) = self.allocate(bitmap.width, bitmap.height)?;
            for row in 0..bitmap.height {
                let src = (row * bitmap.width) as usize;
                let dst = ((y + row) * ATLAS + x) as usize;
                self.pixels[dst..dst + bitmap.width as usize]
                    .copy_from_slice(&bitmap.coverage[src..src + bitmap.width as usize]);
            }
            let d = self.dirty.get_or_insert([x, y, x, y]);
            d[0] = d[0].min(x);
            d[1] = d[1].min(y);
            d[2] = d[2].max(x + bitmap.width);
            d[3] = d[3].max(y + bitmap.height);
            Some(Slot {
                x: x as u16,
                y: y as u16,
                w: bitmap.width as u16,
                h: bitmap.height as u16,
                left: bitmap.left as i16,
                top: bitmap.top as i16,
            })
        });
        self.cache.insert(key, slot);
        slot
    }

    /// Shelf packing with a 1-texel gutter. When the atlas is full it is
    /// cleared (and [`Text::reset`] set), which only happens if the UI ran
    /// through an unusual number of sizes or scripts.
    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (w, h) = (w + 1, h + 1);
        if w > ATLAS || h > ATLAS {
            return None;
        }
        if self.shelf_x + w > ATLAS {
            self.shelf_y += self.shelf_h;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > ATLAS {
            self.cache.clear();
            self.pixels.fill(0);
            self.dirty = Some([0, 0, ATLAS, ATLAS]);
            self.shelf_x = 0;
            self.shelf_y = 0;
            self.shelf_h = 0;
            self.reset = true;
        }
        let at = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Some(at)
    }

    /// `text` shortened with "…" to fit `max` pixels.
    pub fn elide(&mut self, text: &str, px: f32, max: f32) -> String {
        if self.measure(text, px) <= max {
            return text.to_string();
        }
        let budget = max - self.advance('…', px);
        let mut width = 0.0;
        let mut out = String::new();
        for c in text.chars() {
            width += self.advance(c, px);
            if width > budget {
                break;
            }
            out.push(c);
        }
        out.push('…');
        out
    }

    /// Breaks `text` into lines of at most `max` pixels, at spaces where
    /// possible, and at newlines. Returns byte ranges into `text`; a line
    /// broken at a space ends after that space.
    pub fn wrap(&mut self, text: &str, px: f32, max: f32) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut width = 0.0;
        // Byte index just after the last space in this line, and the width there.
        let mut last_space: Option<(usize, f32)> = None;
        for (i, c) in text.char_indices() {
            if c == '\n' {
                lines.push((start, i));
                start = i + 1;
                width = 0.0;
                last_space = None;
                continue;
            }
            let advance = self.advance(c, px);
            if width + advance > max && i > start {
                match last_space {
                    Some((at, at_width)) => {
                        lines.push((start, at));
                        start = at;
                        width -= at_width;
                    }
                    None => {
                        lines.push((start, i));
                        start = i;
                        width = 0.0;
                    }
                }
                last_space = None;
            }
            width += advance;
            if c == ' ' {
                last_space = Some((i + 1, width));
            }
        }
        lines.push((start, text.len()));
        lines
    }
}

/// Characters that take no space: zero-width (non-)joiners and spaces,
/// direction marks, variation selectors and tag characters.
fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x2060..=0x2064 | 0xFE00..=0xFE0F | 0xE0020..=0xE007F | 0xFEFF)
}

/// Characters better drawn from the colour emoji font than from a text font
/// that happens to have them.
fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF)
}

fn color_id(font: u8, glyph: u16, w: u32) -> ImageId {
    // High bit set: never collides with small app-chosen ids in practice.
    (1 << 63) | ((font as u64) << 40) | ((glyph as u64) << 16) | (w as u64 & 0xFFFF)
}

/// Fallback font files from fontconfig (on Windows, the system's fonts
/// for other scripts), best last (they are popped).
fn fallback_fonts() -> Vec<PathBuf> {
    let mut list = fc_files(&["-s", "sans-serif"]);
    list.extend(windows_fonts(&WINDOWS_FALLBACKS));
    list.reverse();
    list
}

fn emoji_font_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("QUI_EMOJI_FONT") {
        return Some(path.into());
    }
    // Segoe UI Emoji's colour layers (COLR) aren't supported: its emoji
    // come out in one colour.
    fc_files(&["emoji"]).into_iter().chain(windows_fonts(&["seguiemj.ttf"])).next()
}

/// Windows fonts covering other scripts, most preferred first: symbols,
/// Chinese, Japanese, Korean, Indic, Thai and Southeast Asian, African,
/// Hebrew and Arabic, historic scripts. Fonts with CFF outlines are
/// skipped by the parser.
const WINDOWS_FALLBACKS: [&str; 17] = [
    "seguisym.ttf",
    "msyh.ttc",
    "YuGothM.ttc",
    "malgun.ttf",
    "Nirmala.ttc",
    "Nirmala.ttf",
    "LeelawUI.ttf",
    "Ebrima.ttf",
    "gadugi.ttf",
    "himalaya.ttf",
    "mmrtext.ttf",
    "javatext.ttf",
    "arial.ttf",
    "tahoma.ttf",
    "seguihis.ttf",
    "simsun.ttc",
    "mingliub.ttc",
];

/// Those of the named files that the Windows font folder has (nothing
/// elsewhere).
fn windows_fonts(names: &[&str]) -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let dir = PathBuf::from(windir).join("Fonts");
    names.iter().map(|n| dir.join(n)).filter(|p| p.exists()).collect()
}

/// Font files `fc-match` lists for a pattern (nothing without fontconfig).
fn fc_files(args: &[&str]) -> Vec<PathBuf> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let out = std::process::Command::new("fc-match")
            .args(args)
            .arg("--format=%{file}\n")
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(out) = out {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
        }
    }
    let _ = args;
    Vec::new()
}
