//! A small TrueType (`glyf`) reader and rasterizer.
//!
//! Reads `cmap` (formats 4 and 12), `hmtx`, `loca` and `glyf` (simple and
//! composite glyphs) and rasterizes outlines with a signed-area
//! accumulation buffer: every line segment adds its exact coverage to the
//! cells it crosses, and a prefix sum over each row yields anti-aliased
//! coverage. Quadratic curves are flattened adaptively. No hinting.
//!
//! CFF-flavoured OpenType (`OTTO`) is not supported; [`Font::parse`] rejects
//! it so a caller can fall back to another file.
//!
//! Colour bitmap fonts (`CBLC`/`CBDT`, e.g. Noto Color Emoji) are read too:
//! [`Font::color_bitmap`] returns a glyph's PNG, which the caller decodes.

use std::collections::HashMap;
use std::ops::Deref;

/// A font file's bytes: read into memory, or mapped (fallback fonts are
/// large and only a few of their pages are ever touched).
pub enum FontData {
    Owned(Vec<u8>),
    #[cfg(unix)]
    Mapped(memmap2::Mmap),
}

impl Deref for FontData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FontData::Owned(v) => v,
            #[cfg(unix)]
            FontData::Mapped(m) => m,
        }
    }
}

/// A colour glyph's embedded image, with its metrics at `ppem`.
pub struct ColorBitmap<'a> {
    pub png: &'a [u8],
    pub ppem: f32,
    pub width: u32,
    pub height: u32,
    pub bearing_x: f32,
    pub bearing_y: f32,
}

pub struct Font {
    data: FontData,
    /// Offsets of CBLC and CBDT (colour bitmaps).
    color: Option<(usize, usize)>,
    /// The font has `glyf` outlines.
    outlines: bool,
    /// Offset of the font inside `data` (non-zero in collections).
    glyf: usize,
    loca: usize,
    long_loca: bool,
    hmtx: usize,
    num_hmetrics: u16,
    num_glyphs: u16,
    units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    pub line_gap: f32,
    cmap: Cmap,
}

enum Cmap {
    Format4(usize),
    Format12(usize),
}

/// A rasterized glyph: `width × height` coverage bytes, placed `left`
/// pixels right of the pen and `top` pixels above the baseline.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub coverage: Vec<u8>,
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(o)?, *d.get(o + 1)?]))
}
fn i16_at(d: &[u8], o: usize) -> Option<i16> {
    u16_at(d, o).map(|v| v as i16)
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *d.get(o)?,
        *d.get(o + 1)?,
        *d.get(o + 2)?,
        *d.get(o + 3)?,
    ]))
}

impl Font {
    /// Parses a `.ttf` or the first font of a `.ttc`.
    pub fn parse(data: Vec<u8>) -> Option<Font> {
        Self::from_data(FontData::Owned(data))
    }

    /// Opens a font file, mapped into memory on Unix.
    pub fn open(path: &std::path::Path) -> Option<Font> {
        #[cfg(unix)]
        {
            let file = std::fs::File::open(path).ok()?;
            // SAFETY: font files aren't modified while in use; if one is,
            // glyphs come out wrong, the reads stay in bounds.
            let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
            Self::from_data(FontData::Mapped(map))
        }
        #[cfg(not(unix))]
        {
            Self::parse(std::fs::read(path).ok()?)
        }
    }

    /// Whether glyphs of this font are colour images.
    pub fn is_color(&self) -> bool {
        self.color.is_some() && !self.outlines
    }

    pub fn from_data(data: FontData) -> Option<Font> {
        let d = &data;
        let mut base = 0usize;
        if d.get(0..4)? == b"ttcf" {
            base = u32_at(d, 12)? as usize;
        }
        let version = u32_at(d, base)?;
        if version != 0x0001_0000 && version != u32::from_be_bytes(*b"true") {
            return None; // CFF outlines or not a font.
        }
        let num_tables = u16_at(d, base + 4)? as usize;
        let mut tables: HashMap<[u8; 4], usize> = HashMap::new();
        for i in 0..num_tables {
            let rec = base + 12 + i * 16;
            let tag: [u8; 4] = d.get(rec..rec + 4)?.try_into().ok()?;
            tables.insert(tag, u32_at(d, rec + 8)? as usize);
        }
        let head = *tables.get(b"head")?;
        let hhea = *tables.get(b"hhea")?;
        let maxp = *tables.get(b"maxp")?;
        let cmap = *tables.get(b"cmap")?;

        let units_per_em = u16_at(d, head + 18)? as f32;
        let long_loca = i16_at(d, head + 50)? != 0;

        // Prefer full Unicode (format 12), then the BMP (format 4).
        let mut best: Option<(u8, Cmap)> = None;
        let count = u16_at(d, cmap + 2)? as usize;
        for i in 0..count {
            let rec = cmap + 4 + i * 8;
            let platform = u16_at(d, rec)?;
            let encoding = u16_at(d, rec + 2)?;
            let sub = cmap + u32_at(d, rec + 4)? as usize;
            let format = u16_at(d, sub)?;
            let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
            let candidate = match format {
                12 if unicode => (2, Cmap::Format12(sub)),
                4 if unicode => (1, Cmap::Format4(sub)),
                _ => continue,
            };
            if best.as_ref().is_none_or(|(rank, _)| candidate.0 > *rank) {
                best = Some(candidate);
            }
        }

        let color = tables.get(b"CBLC").zip(tables.get(b"CBDT")).map(|(a, b)| (*a, *b));
        let outlines = tables.contains_key(b"glyf") && tables.contains_key(b"loca");
        if !outlines && color.is_none() {
            return None;
        }
        Some(Font {
            color,
            outlines,
            glyf: tables.get(b"glyf").copied().unwrap_or(0),
            loca: tables.get(b"loca").copied().unwrap_or(0),
            hmtx: *tables.get(b"hmtx")?,
            long_loca,
            num_hmetrics: u16_at(d, hhea + 34)?,
            num_glyphs: u16_at(d, maxp + 4)?,
            units_per_em,
            ascender: i16_at(d, hhea + 4)? as f32 / units_per_em,
            descender: i16_at(d, hhea + 6)? as f32 / units_per_em,
            line_gap: i16_at(d, hhea + 8)? as f32 / units_per_em,
            cmap: best?.1,
            data,
        })
    }

    /// The glyph for a character, or `None` (glyph 0 is "missing").
    pub fn glyph(&self, c: char) -> Option<u16> {
        let d = &self.data;
        let c = c as u32;
        let id = match self.cmap {
            Cmap::Format12(sub) => {
                let groups = u32_at(d, sub + 12)? as usize;
                let (mut lo, mut hi) = (0, groups);
                let mut found = 0;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let g = sub + 16 + mid * 12;
                    let start = u32_at(d, g)?;
                    let end = u32_at(d, g + 4)?;
                    if c < start {
                        hi = mid;
                    } else if c > end {
                        lo = mid + 1;
                    } else {
                        found = u32_at(d, g + 8)? + (c - start);
                        break;
                    }
                }
                found
            }
            Cmap::Format4(sub) => {
                if c > 0xFFFF {
                    return None;
                }
                let seg_x2 = u16_at(d, sub + 6)? as usize;
                let ends = sub + 14;
                let starts = ends + seg_x2 + 2;
                let deltas = starts + seg_x2;
                let ranges = deltas + seg_x2;
                // Binary search the first segment whose end >= c.
                let (mut lo, mut hi) = (0, seg_x2 / 2);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if (u16_at(d, ends + mid * 2)? as u32) < c {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let seg = lo;
                if seg >= seg_x2 / 2 || (u16_at(d, starts + seg * 2)? as u32) > c {
                    return None;
                }
                let start = u16_at(d, starts + seg * 2)? as u32;
                let delta = u16_at(d, deltas + seg * 2)? as u32;
                let range = u16_at(d, ranges + seg * 2)? as usize;
                if range == 0 {
                    (c + delta) & 0xFFFF
                } else {
                    let at = ranges + seg * 2 + range + (c - start) as usize * 2;
                    let g = u16_at(d, at)? as u32;
                    if g == 0 { 0 } else { (g + delta) & 0xFFFF }
                }
            }
        };
        (id != 0 && id < self.num_glyphs as u32).then_some(id as u16)
    }

    /// Horizontal advance in em units.
    pub fn advance(&self, glyph: u16) -> f32 {
        let i = glyph.min(self.num_hmetrics.saturating_sub(1)) as usize;
        u16_at(&self.data, self.hmtx + i * 4).unwrap_or(0) as f32 / self.units_per_em
    }

    fn glyph_range(&self, glyph: u16) -> Option<(usize, usize)> {
        if !self.outlines {
            return None;
        }
        let g = glyph as usize;
        let (a, b) = if self.long_loca {
            (
                u32_at(&self.data, self.loca + g * 4)? as usize,
                u32_at(&self.data, self.loca + g * 4 + 4)? as usize,
            )
        } else {
            (
                u16_at(&self.data, self.loca + g * 2)? as usize * 2,
                u16_at(&self.data, self.loca + g * 2 + 2)? as usize * 2,
            )
        };
        (b > a).then_some((self.glyf + a, self.glyf + b))
    }

    /// Appends the glyph's contours (font units, y up) to `out`.
    fn outline(&self, glyph: u16, transform: [f32; 6], depth: u32, out: &mut Vec<Contour>) -> Option<()> {
        let Some((start, _end)) = self.glyph_range(glyph) else {
            return Some(()); // Empty glyph (e.g. space).
        };
        let d = &self.data;
        let contours = i16_at(d, start)?;
        let [a, b, c, dd, e, f] = transform;
        let apply = |x: f32, y: f32| (a * x + c * y + e, b * x + dd * y + f);

        if contours >= 0 {
            let n = contours as usize;
            let mut ends = Vec::with_capacity(n);
            for i in 0..n {
                ends.push(u16_at(d, start + 10 + i * 2)? as usize);
            }
            let points = ends.last().map_or(0, |e| e + 1);
            let instr = u16_at(d, start + 10 + n * 2)? as usize;
            let mut p = start + 12 + n * 2 + instr;

            let mut flags = Vec::with_capacity(points);
            while flags.len() < points {
                let flag = *d.get(p)?;
                p += 1;
                flags.push(flag);
                if flag & 8 != 0 {
                    let repeat = *d.get(p)?;
                    p += 1;
                    for _ in 0..repeat {
                        flags.push(flag);
                    }
                }
            }
            flags.truncate(points);

            let mut read_coords = |short: u8, same: u8| -> Option<Vec<i32>> {
                let mut v = 0i32;
                let mut out = Vec::with_capacity(points);
                for &flag in &flags {
                    if flag & short != 0 {
                        let delta = *d.get(p)? as i32;
                        p += 1;
                        v += if flag & same != 0 { delta } else { -delta };
                    } else if flag & same == 0 {
                        v += i16_at(d, p)? as i32;
                        p += 2;
                    }
                    out.push(v);
                }
                Some(out)
            };
            let xs = read_coords(2, 16)?;
            let ys = read_coords(4, 32)?;

            let mut first = 0;
            for &end in &ends {
                if end < first || end >= points {
                    return None;
                }
                let contour: Contour = (first..=end)
                    .map(|i| {
                        let (x, y) = apply(xs[i] as f32, ys[i] as f32);
                        (x, y, flags[i] & 1 != 0)
                    })
                    .collect();
                out.push(contour);
                first = end + 1;
            }
            Some(())
        } else {
            if depth > 8 {
                return None;
            }
            let mut p = start + 10;
            loop {
                let flags = u16_at(d, p)?;
                let component = u16_at(d, p + 2)?;
                p += 4;
                let (dx, dy) = if flags & 1 != 0 {
                    let v = (i16_at(d, p)? as f32, i16_at(d, p + 2)? as f32);
                    p += 4;
                    v
                } else {
                    let v = (*d.get(p)? as i8 as f32, *d.get(p + 1)? as i8 as f32);
                    p += 2;
                    v
                };
                // Point-matched placement (flag 2 unset) is rare; place at 0,0.
                let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
                let f2 = |o: usize| i16_at(d, o).map(|v| v as f32 / 16384.0);
                let (ma, mb, mc, md) = if flags & 8 != 0 {
                    let s = f2(p)?;
                    p += 2;
                    (s, 0.0, 0.0, s)
                } else if flags & 0x40 != 0 {
                    let v = (f2(p)?, 0.0, 0.0, f2(p + 2)?);
                    p += 4;
                    v
                } else if flags & 0x80 != 0 {
                    let v = (f2(p)?, f2(p + 2)?, f2(p + 4)?, f2(p + 6)?);
                    p += 8;
                    v
                } else {
                    (1.0, 0.0, 0.0, 1.0)
                };
                // Compose: outer ∘ component.
                let t = [
                    a * ma + c * mb,
                    b * ma + dd * mb,
                    a * mc + c * md,
                    b * mc + dd * md,
                    a * dx + c * dy + e,
                    b * dx + dd * dy + f,
                ];
                self.outline(component, t, depth + 1, out)?;
                if flags & 0x20 == 0 {
                    break;
                }
            }
            Some(())
        }
    }

    /// A colour glyph's PNG from the strike closest to (and preferably not
    /// below) `px` pixels per em.
    pub fn color_bitmap(&self, glyph: u16, px: f32) -> Option<ColorBitmap<'_>> {
        let (cblc, cbdt) = self.color?;
        let d: &[u8] = &self.data;
        let sizes = u32_at(d, cblc + 4)? as usize;
        // (ppem, record offset) of the best strike that has this glyph.
        let mut best: Option<(u8, usize)> = None;
        for i in 0..sizes.min(64) {
            let rec = cblc + 8 + i * 48;
            let start = u16_at(d, rec + 40)?;
            let end = u16_at(d, rec + 42)?;
            if glyph < start || glyph > end {
                continue;
            }
            let ppem = *d.get(rec + 45)?;
            let better = match best {
                None => true,
                Some((b, _)) => {
                    let (bp, np) = (b as f32, ppem as f32);
                    if (bp >= px) != (np >= px) { np >= px } else if np >= px { np < bp } else { np > bp }
                }
            };
            if better {
                best = Some((ppem, rec));
            }
        }
        let (ppem, rec) = best?;
        let array = cblc + u32_at(d, rec)? as usize;
        let count = u32_at(d, rec + 8)? as usize;
        for i in 0..count.min(4096) {
            let entry = array + i * 8;
            let first = u16_at(d, entry)?;
            let last = u16_at(d, entry + 2)?;
            if glyph < first || glyph > last {
                continue;
            }
            let sub = array + u32_at(d, entry + 4)? as usize;
            let index_format = u16_at(d, sub)?;
            let image_format = u16_at(d, sub + 2)?;
            let image_data = cbdt + u32_at(d, sub + 4)? as usize;
            let k = (glyph - first) as usize;
            // (offset of the glyph's data, big metrics from the index).
            let (offset, index_metrics) = match index_format {
                1 => (u32_at(d, sub + 8 + k * 4)? as usize, None),
                3 => (u16_at(d, sub + 8 + k * 2)? as usize, None),
                2 => (u32_at(d, sub + 8)? as usize * k, Some(sub + 12)),
                4 => {
                    let n = u32_at(d, sub + 8)? as usize;
                    let pos = (0..n).find(|&j| u16_at(d, sub + 12 + j * 4) == Some(glyph))?;
                    (u16_at(d, sub + 12 + pos * 4 + 2)? as usize, None)
                }
                5 => {
                    let size = u32_at(d, sub + 8)? as usize;
                    let n = u32_at(d, sub + 20)? as usize;
                    let pos = (0..n).find(|&j| u16_at(d, sub + 24 + j * 2) == Some(glyph))?;
                    (size * pos, Some(sub + 12))
                }
                _ => return None,
            };
            let at = image_data + offset;
            // (height, width, bearing x, bearing y) and where the PNG length is.
            let (h, w, bx, by, len_at) = match image_format {
                17 => (*d.get(at)?, *d.get(at + 1)?, *d.get(at + 2)? as i8, *d.get(at + 3)? as i8, at + 5),
                18 => (*d.get(at)?, *d.get(at + 1)?, *d.get(at + 2)? as i8, *d.get(at + 3)? as i8, at + 8),
                19 => {
                    let m = index_metrics?;
                    (*d.get(m)?, *d.get(m + 1)?, *d.get(m + 2)? as i8, *d.get(m + 3)? as i8, at)
                }
                _ => return None,
            };
            let len = u32_at(d, len_at)? as usize;
            let png = d.get(len_at + 4..len_at + 4 + len)?;
            return Some(ColorBitmap {
                png,
                ppem: ppem as f32,
                width: w as u32,
                height: h as u32,
                bearing_x: bx as f32,
                bearing_y: by as f32,
            });
        }
        None
    }

    /// Rasterizes a glyph at `px` pixels per em, shifted right by
    /// `x_offset` (0..1, for subpixel positioning).
    pub fn rasterize(&self, glyph: u16, px: f32, x_offset: f32) -> Option<Bitmap> {
        let mut contours = Vec::new();
        self.outline(glyph, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], 0, &mut contours)?;
        let scale = px / self.units_per_em;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for contour in &mut contours {
            for point in contour.iter_mut() {
                point.0 = point.0 * scale + x_offset;
                point.1 *= scale;
                x0 = x0.min(point.0);
                x1 = x1.max(point.0);
                y0 = y0.min(point.1);
                y1 = y1.max(point.1);
            }
        }
        if x0 > x1 {
            return None; // No outline.
        }
        let left = x0.floor() as i32;
        let top = y1.ceil() as i32;
        let width = (x1.ceil() as i32 - left).max(1) as u32;
        let height = (top - y0.floor() as i32).max(1) as u32;

        let mut raster = Raster::new(width as usize, height as usize);
        // To bitmap space: x from the left edge, y down from the top.
        let to = |x: f32, y: f32| (x - left as f32, top as f32 - y);
        for contour in &contours {
            for_each_segment(contour, |seg| match seg {
                Segment::Line(a, b) => raster.line(to(a.0, a.1), to(b.0, b.1)),
                Segment::Quad(a, b, c) => raster.quad(to(a.0, a.1), to(b.0, b.1), to(c.0, c.1)),
            });
        }
        Some(Bitmap {
            width,
            height,
            left,
            top,
            coverage: raster.finish(),
        })
    }
}

type Contour = Vec<(f32, f32, bool)>;
type Pt = (f32, f32);

enum Segment {
    Line(Pt, Pt),
    Quad(Pt, Pt, Pt),
}

/// Walks a TrueType contour, inserting the implied on-curve points between
/// consecutive off-curve points.
fn for_each_segment(contour: &Contour, mut f: impl FnMut(Segment)) {
    let n = contour.len();
    if n < 2 {
        return;
    }
    let mid = |a: (f32, f32, bool), b: (f32, f32, bool)| ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    // Start at an on-curve point (or an implied one).
    let (start, first) = match contour.iter().position(|p| p.2) {
        Some(i) => (i, (contour[i].0, contour[i].1)),
        None => (0, mid(contour[0], contour[1])),
    };
    let mut current = first;
    let mut control: Option<Pt> = None;
    for k in 1..=n {
        let p = contour[(start + k) % n];
        let point = (p.0, p.1);
        if p.2 {
            match control.take() {
                Some(c) => f(Segment::Quad(current, c, point)),
                None => f(Segment::Line(current, point)),
            }
            current = point;
        } else {
            if let Some(c) = control {
                let m = ((c.0 + point.0) * 0.5, (c.1 + point.1) * 0.5);
                f(Segment::Quad(current, c, m));
                current = m;
            }
            control = Some(point);
        }
    }
    match control {
        Some(c) => f(Segment::Quad(current, c, first)),
        None if current != first => f(Segment::Line(current, first)),
        None => {}
    }
}

/// Signed-area accumulation rasterizer.
struct Raster {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Raster {
    fn new(w: usize, h: usize) -> Self {
        Raster {
            w,
            h,
            a: vec![0.0; w * h + 4],
        }
    }

    fn line(&mut self, p0: Pt, p1: Pt) {
        if p0.1 == p1.1 {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let y_start = p0.1.max(0.0);
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let y_end = (p1.1.ceil() as usize).min(self.h);
        let wmax = self.w as f32;
        for y in (y_start as usize)..y_end {
            let row = y * self.w;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            // Clamp to the bitmap; points are inside by construction, this
            // only guards against rounding at the edges.
            let (x0, x1) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let (x0, x1) = (x0.clamp(0.0, wmax), x1.clamp(0.0, wmax));
            let x0f = x0.floor();
            let x0i = x0f as usize;
            let x1c = x1.ceil();
            let x1i = x1c as usize;
            if x1i <= x0i + 1 {
                let xmf = 0.5 * (x0 + x1) - x0f;
                self.a[row + x0i] += d - d * xmf;
                self.a[row + x0i + 1] += d * xmf;
            } else {
                let s = (x1 - x0).recip();
                let x0frac = x0 - x0f;
                let a0 = 0.5 * s * (1.0 - x0frac) * (1.0 - x0frac);
                let x1frac = x1 - x1c + 1.0;
                let am = 0.5 * s * x1frac * x1frac;
                self.a[row + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.a[row + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0frac);
                    self.a[row + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.a[row + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.a[row + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.a[row + x1i] += d * am;
            }
            x = xnext;
        }
    }

    fn quad(&mut self, p0: Pt, p1: Pt, p2: Pt) {
        let dev = (p0.0 - 2.0 * p1.0 + p2.0, p0.1 - 2.0 * p1.1 + p2.1);
        let devsq = dev.0 * dev.0 + dev.1 * dev.1;
        if devsq < 0.333 {
            self.line(p0, p2);
            return;
        }
        let n = 1 + (3.0 * devsq).sqrt().sqrt().floor() as usize;
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let p = (
                mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0,
                mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1,
            );
            self.line(prev, p);
            prev = p;
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w * self.h);
        // One running sum over the whole buffer: a closed contour's deltas
        // cancel within each row, except for the spill into the cell just
        // past an edge at x = w, which is the next row's first cell.
        let mut acc = 0.0f32;
        for &value in self.a.iter().take(self.w * self.h) {
            acc += value;
            out.push((acc.abs().min(1.0) * 255.0 + 0.5) as u8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_coverage() {
        // A 2×2 square at (1,1): full coverage inside, none outside.
        let mut r = Raster::new(4, 4);
        let pts = [(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0)];
        for i in 0..4 {
            r.line(pts[i], pts[(i + 1) % 4]);
        }
        let c = r.finish();
        assert_eq!(c[5], 255);
        assert_eq!(c[10], 255);
        assert_eq!(c[0], 0);
        assert_eq!(c[15], 0);
    }

    #[test]
    fn half_pixel_edge() {
        let mut r = Raster::new(2, 1);
        let pts = [(0.5, 0.0), (2.0, 0.0), (2.0, 1.0), (0.5, 1.0)];
        for i in 0..4 {
            r.line(pts[i], pts[(i + 1) % 4]);
        }
        let c = r.finish();
        assert!((c[0] as i32 - 128).abs() <= 1, "{}", c[0]);
        assert_eq!(c[1], 255);
    }

    #[test]
    fn system_font() {
        // Smoke test against whatever font the system has.
        let Some(path) = crate::text::default_font_paths()
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        let font = Font::parse(std::fs::read(path).unwrap()).unwrap();
        let g = font.glyph('A').unwrap();
        let bmp = font.rasterize(g, 32.0, 0.0).unwrap();
        assert!(bmp.width > 10 && bmp.height > 15);
        assert!(bmp.coverage.contains(&255));
        assert!(font.glyph('\u{10FFFF}').is_none());
        // Composite glyph (Ä is often composed of A + diaeresis).
        let g = font.glyph('Ä').unwrap();
        assert!(font.rasterize(g, 32.0, 0.0).unwrap().height > bmp.height);
    }
}
