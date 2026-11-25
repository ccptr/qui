//! Images: an RGBA atlas next to the glyph atlas, for icons, pictures and
//! colour emoji.
//!
//! The app decodes an image (at the pixel size it will be drawn) and
//! [`Images::insert`]s it under an id it chooses; [`crate::Ui::image`]
//! draws it. Pixels are kept on the CPU side as well, so when the atlas
//! fills up it is simply repacked with what the current frame uses (the
//! frame is then built again, as for the glyph atlas). Images not drawn for
//! a while are dropped once the CPU copies exceed [`Images::budget`].

use std::collections::HashMap;
use std::sync::Arc;

/// Atlas side length (RGBA8: 4 MiB on the GPU).
pub const IMAGE_ATLAS: u32 = 1024;
/// Texels between images, so filtering never reads a neighbour.
const GUTTER: u32 = 2;

pub type ImageId = u64;

struct Entry {
    w: u32,
    h: u32,
    /// Premultiplied RGBA8.
    data: Arc<[u8]>,
    slot: Option<(u32, u32)>,
    /// Frame counter of the last draw.
    used: u64,
}

/// A rectangle of the atlas waiting to be copied to the GPU.
pub struct Upload {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub data: Arc<[u8]>,
}

pub struct Images {
    entries: HashMap<ImageId, Entry>,
    pub(crate) pending: Vec<Upload>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    frame: u64,
    bytes: usize,
    /// Bytes of CPU-side pixels kept before unused images are dropped.
    pub budget: usize,
    /// Set when the atlas was repacked mid-frame: build the frame again.
    pub reset: bool,
}

impl Default for Images {
    fn default() -> Self {
        Images {
            entries: HashMap::new(),
            pending: Vec::new(),
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            frame: 0,
            bytes: 0,
            budget: 8 << 20,
            reset: false,
        }
    }
}

impl Images {
    /// Adds (or replaces) an image from straight-alpha RGBA8 pixels.
    /// Images larger than the atlas are scaled down to fit.
    pub fn insert(&mut self, id: ImageId, w: u32, h: u32, mut rgba: Vec<u8>) {
        let (mut w, mut h) = (w.max(1), h.max(1));
        if rgba.len() != (w * h * 4) as usize {
            return;
        }
        let max = IMAGE_ATLAS - GUTTER;
        if w > max || h > max {
            let f = max as f32 / w.max(h) as f32;
            let (nw, nh) = (((w as f32 * f) as u32).max(1), ((h as f32 * f) as u32).max(1));
            rgba = resize(&rgba, w, h, nw, nh);
            (w, h) = (nw, nh);
        }
        premultiply(&mut rgba);
        self.remove(id);
        self.bytes += rgba.len();
        self.entries.insert(id, Entry { w, h, data: rgba.into(), slot: None, used: self.frame });
        self.trim();
    }

    pub fn contains(&self, id: ImageId) -> bool {
        self.entries.contains_key(&id)
    }

    /// The image's size in pixels.
    pub fn size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.entries.get(&id).map(|e| (e.w, e.h))
    }

    pub fn remove(&mut self, id: ImageId) {
        if let Some(e) = self.entries.remove(&id) {
            self.bytes -= e.data.len();
        }
    }

    /// Drops every image (e.g. when the window is hidden).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
        self.bytes = 0;
        self.repack_reset();
        self.reset = false;
    }

    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        self.reset = false;
    }

    /// The atlas rectangle of an image, uploading it on first use.
    pub(crate) fn slot(&mut self, id: ImageId) -> Option<[u32; 4]> {
        let frame = self.frame;
        let e = self.entries.get_mut(&id)?;
        e.used = frame;
        if let Some((x, y)) = e.slot {
            return Some([x, y, e.w, e.h]);
        }
        let (w, h) = (e.w, e.h);
        let (x, y) = match self.allocate(w, h) {
            Some(at) => at,
            None => {
                // Full: start over with only what this frame draws.
                self.repack_reset();
                self.reset = true;
                self.allocate(w, h)?
            }
        };
        let e = self.entries.get_mut(&id)?;
        e.slot = Some((x, y));
        self.pending.push(Upload { x, y, w, h, data: e.data.clone() });
        Some([x, y, w, h])
    }

    fn repack_reset(&mut self) {
        for e in self.entries.values_mut() {
            e.slot = None;
        }
        self.pending.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_h = 0;
    }

    fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (w, h) = (w + GUTTER, h + GUTTER);
        if w > IMAGE_ATLAS || h > IMAGE_ATLAS {
            return None;
        }
        if self.shelf_x + w > IMAGE_ATLAS {
            self.shelf_y += self.shelf_h;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > IMAGE_ATLAS {
            return None;
        }
        let at = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Some(at)
    }

    /// Drops the least recently drawn images (never this frame's) while
    /// over budget.
    fn trim(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        let mut by_age: Vec<(u64, ImageId)> =
            self.entries.iter().filter(|(_, e)| e.used + 1 < self.frame).map(|(id, e)| (e.used, *id)).collect();
        by_age.sort_unstable();
        for (_, id) in by_age {
            if self.bytes <= self.budget {
                break;
            }
            self.remove(id);
        }
    }
}

/// Decodes a PNG into straight-alpha RGBA8 (`None` without the `png`
/// feature or for a broken file).
pub fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    #[cfg(feature = "png")]
    {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());
        let (w, h) = (info.width, info.height);
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect(),
            png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|&[g, a]| [g, g, g, a]).collect(),
            png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => return None,
        };
        (rgba.len() == (w * h * 4) as usize).then_some((w, h, rgba))
    }
    #[cfg(not(feature = "png"))]
    {
        let _ = bytes;
        None
    }
}

fn premultiply(rgba: &mut [u8]) {
    for [r, g, b, a] in rgba.as_chunks_mut::<4>().0 {
        let a = *a as u32;
        if a < 255 {
            for c in [r, g, b] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

/// Resamples straight-alpha RGBA8 to `nw × nh`: an area average when
/// shrinking (sharp, no aliasing), bilinear when enlarging. Colour is
/// weighted by alpha, so transparent pixels don't darken edges.
pub fn resize(src: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let (nw, nh) = (nw.max(1), nh.max(1));
    if (w, h) == (nw, nh) {
        return src.to_vec();
    }
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let sx = w as f32 / nw as f32;
    let sy = h as f32 / nh as f32;
    let px = |x: u32, y: u32| -> [f32; 4] {
        let i = ((y.min(h - 1) * w + x.min(w - 1)) * 4) as usize;
        let a = src[i + 3] as f32;
        [src[i] as f32 * a, src[i + 1] as f32 * a, src[i + 2] as f32 * a, a]
    };
    for oy in 0..nh {
        for ox in 0..nw {
            let mut acc = [0f32; 4];
            if sx >= 1.0 && sy >= 1.0 {
                // Box filter over the source area this pixel covers.
                let (x0, x1) = (ox as f32 * sx, (ox + 1) as f32 * sx);
                let (y0, y1) = (oy as f32 * sy, (oy + 1) as f32 * sy);
                let mut total = 0.0;
                let mut y = y0.floor();
                while y < y1 {
                    let wy = (y + 1.0).min(y1) - y.max(y0);
                    let mut x = x0.floor();
                    while x < x1 {
                        let wx = (x + 1.0).min(x1) - x.max(x0);
                        let p = px(x as u32, y as u32);
                        let wgt = wx * wy;
                        for k in 0..4 {
                            acc[k] += p[k] * wgt;
                        }
                        total += wgt;
                        x += 1.0;
                    }
                    y += 1.0;
                }
                for v in &mut acc {
                    *v /= total.max(1e-6);
                }
            } else {
                let fx = ((ox as f32 + 0.5) * sx - 0.5).max(0.0);
                let fy = ((oy as f32 + 0.5) * sy - 0.5).max(0.0);
                let (x, y) = (fx as u32, fy as u32);
                let (tx, ty) = (fx - x as f32, fy - y as f32);
                let (a, b, c, d) = (px(x, y), px(x + 1, y), px(x, y + 1), px(x + 1, y + 1));
                for k in 0..4 {
                    acc[k] = (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty) + (c[k] * (1.0 - tx) + d[k] * tx) * ty;
                }
            }
            let o = ((oy * nw + ox) * 4) as usize;
            let a = acc[3];
            if a > 0.0 {
                out[o] = (acc[0] / a).round().clamp(0.0, 255.0) as u8;
                out[o + 1] = (acc[1] / a).round().clamp(0.0, 255.0) as u8;
                out[o + 2] = (acc[2] / a).round().clamp(0.0, 255.0) as u8;
            }
            out[o + 3] = a.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repacks_when_full() {
        let mut images = Images::default();
        images.begin_frame();
        let side = IMAGE_ATLAS / 2 - GUTTER;
        for id in 0..5 {
            images.insert(id, side, side, vec![255; (side * side * 4) as usize]);
        }
        for id in 0..4 {
            assert!(images.slot(id).is_some());
        }
        assert!(!images.reset);
        // A fifth doesn't fit: repacked, and the frame must be rebuilt.
        assert!(images.slot(4).is_some());
        assert!(images.reset);
    }

    #[test]
    fn resize_keeps_transparent_edges_clean() {
        // White opaque pixel next to a transparent black one.
        let src = [255, 255, 255, 255, 0, 0, 0, 0];
        let out = resize(&src, 2, 1, 1, 1);
        assert_eq!(&out[..3], &[255, 255, 255]);
        assert!((out[3] as i32 - 128).abs() <= 1);
    }
}
