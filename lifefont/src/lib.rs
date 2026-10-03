// SPDX-License-Identifier: GPL-3.0-or-later
// Glyph metrics and coverage bitmaps for the life* components.
//
// The API deliberately mirrors the four fontdue calls the components used
// (from_bytes, metrics, horizontal_line_metrics, rasterize) with the same
// conventions, so switching was an import change:
//   * `px` is the em size in pixels (not ab_glyph's ascent-to-descent height);
//   * bitmaps are row-major, top row first, one coverage byte per pixel;
//   * `xmin` is the bitmap's left edge from the pen, `ymin` its BOTTOM edge
//     from the baseline, positive up.
//
// What changed is memory: ab_glyph keeps the font as bytes and reads one
// outline when asked, where fontdue built all of them at load. A font opened
// from a path is memory-mapped, not read: its pages are the shared page cache,
// only the glyphs drawn are ever touched, and a 20 MB CJK fallback costs a
// few pages rather than 20 MB of heap per process.

use ab_glyph::{Font as _, FontRef, FontVec, GlyphId, PxScale};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    /// Negative: below the baseline.
    pub descent: f32,
    pub line_gap: f32,
    /// ascent - descent + line_gap: baseline to baseline.
    pub new_line_size: f32,
}

/// A read-only private mapping of a whole font file.
struct Map {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and never written through, so sharing it
// across threads is sharing immutable bytes.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    fn open(path: &str) -> std::io::Result<Map> {
        use std::os::fd::AsRawFd;
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len() as usize;
        if len == 0 {
            return Err(std::io::Error::other("empty file"));
        }
        // SAFETY: a fresh PROT_READ/MAP_PRIVATE mapping of a file we opened;
        // checked for MAP_FAILED below. The fd may close once it's mapped.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Map { ptr, len })
    }

    /// SAFETY: the slice must not outlive `self` (Face drops it first).
    unsafe fn bytes(&self) -> &'static [u8] {
        std::slice::from_raw_parts(self.ptr as *const u8, self.len)
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what open() mapped, once.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

enum Face {
    Owned(FontVec),
    /// Fields drop in order: the font borrowing the map goes first.
    #[allow(dead_code)] // the Map is held only to unmap on drop
    Mapped(FontRef<'static>, Map),
}

pub struct Font {
    face: Face,
    units_per_em: f32,
}

impl Font {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Font, String> {
        Font::from_bytes_index(bytes, 0)
    }

    /// `index` picks a face inside a collection (.ttc), as fontconfig reports it.
    pub fn from_bytes_index(bytes: Vec<u8>, index: u32) -> Result<Font, String> {
        let font = FontVec::try_from_vec_and_index(bytes, index).map_err(|e| e.to_string())?;
        Font::new(Face::Owned(font))
    }

    pub fn from_path(path: &str) -> Result<Font, String> {
        Font::from_path_index(path, 0)
    }

    pub fn from_path_index(path: &str, index: u32) -> Result<Font, String> {
        let map = Map::open(path).map_err(|e| format!("cannot read font {path}: {e}"))?;
        // SAFETY: Face::Mapped keeps the map alive for as long as the font.
        let bytes = unsafe { map.bytes() };
        let font = FontRef::try_from_slice_and_index(bytes, index).map_err(|e| format!("cannot parse font {path}: {e}"))?;
        Font::new(Face::Mapped(font, map))
    }

    fn new(face: Face) -> Result<Font, String> {
        let mut f = Font { face, units_per_em: 1.0 };
        f.units_per_em = f.f().units_per_em().filter(|u| *u > 0.0).ok_or("font has no units-per-em")?;
        Ok(f)
    }

    fn f(&self) -> &dyn ab_glyph::Font {
        match &self.face {
            Face::Owned(v) => v,
            Face::Mapped(r, _) => r,
        }
    }

    /// Whether the font draws `ch` itself (rather than its .notdef box).
    pub fn has_glyph(&self, ch: char) -> bool {
        self.id(ch).0 != 0
    }

    /// Font units -> pixels at em size `px`.
    fn k(&self, px: f32) -> f32 {
        px / self.units_per_em
    }

    /// ab_glyph scales by ascent-to-descent height; convert from em size.
    fn scale(&self, px: f32) -> PxScale {
        PxScale::from(self.f().height_unscaled() * self.k(px))
    }

    fn id(&self, ch: char) -> GlyphId {
        self.f().glyph_id(ch)
    }

    pub fn horizontal_line_metrics(&self, px: f32) -> Option<LineMetrics> {
        let k = self.k(px);
        let (ascent, descent) = (self.f().ascent_unscaled() * k, self.f().descent_unscaled() * k);
        let line_gap = self.f().line_gap_unscaled() * k;
        (ascent != 0.0 || descent != 0.0).then_some(LineMetrics {
            ascent,
            descent,
            line_gap,
            new_line_size: ascent - descent + line_gap,
        })
    }

    pub fn metrics(&self, ch: char, px: f32) -> Metrics {
        self.rasterize_inner(ch, px, false).0
    }

    pub fn rasterize(&self, ch: char, px: f32) -> (Metrics, Vec<u8>) {
        self.rasterize_inner(ch, px, true)
    }

    fn rasterize_inner(&self, ch: char, px: f32, draw: bool) -> (Metrics, Vec<u8>) {
        let id = self.id(ch);
        let advance_width = self.f().h_advance_unscaled(id) * self.k(px);
        let glyph = id.with_scale(self.scale(px));
        // outline_glyph is generic, so not callable through `dyn Font`.
        let outlined = match &self.face {
            Face::Owned(v) => v.outline_glyph(glyph),
            Face::Mapped(r, _) => r.outline_glyph(glyph),
        };
        let Some(outlined) = outlined else {
            // Whitespace and empty glyphs: an advance and nothing to draw.
            return (Metrics { advance_width, ..Metrics::default() }, Vec::new());
        };
        let b = outlined.px_bounds(); // whole pixels, y down from the baseline
        let (width, height) = ((b.max.x - b.min.x) as usize, (b.max.y - b.min.y) as usize);
        let m = Metrics { xmin: b.min.x as i32, ymin: -(b.max.y as i32), width, height, advance_width };
        if !draw {
            return (m, Vec::new());
        }
        let mut cov = vec![0u8; width * height];
        outlined.draw(|x, y, c| {
            let (x, y) = (x as usize, y as usize);
            if x < width && y < height {
                cov[y * width + x] = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });
        (m, cov)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NERD: &str = "/usr/share/fonts/TTF/ShureTechMonoNerdFontMono-Regular.ttf";

    fn both() -> Option<(Font, fontdue::Font)> {
        let bytes = std::fs::read(NERD).ok()?; // not installed (CI): skip
        let a = Font::from_bytes(bytes.clone()).unwrap();
        let b = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).unwrap();
        Some((a, b))
    }

    /// Same metrics and (to rasterizer rounding) the same pixels as fontdue,
    /// across text, box drawing and the shade blocks lifelock uses.
    #[test]
    fn matches_fontdue() {
        let Some((ours, fd)) = both() else { return };
        let chars = (' '..='~').chain("─│┌┐└┘├┤┬┴┼═║╔╗╚╝━┃╭╮╯╰░▒▓█—…é".chars());
        for px in [12.0, 15.0, 22.0, 48.0] {
            let (la, lb) = (ours.horizontal_line_metrics(px).unwrap(), fd.horizontal_line_metrics(px).unwrap());
            assert!((la.ascent - lb.ascent).abs() < 0.01, "ascent {la:?} {lb:?}");
            assert!((la.descent - lb.descent).abs() < 0.01, "descent {la:?} {lb:?}");
            assert!((la.new_line_size - lb.new_line_size).abs() < 0.01);
            for ch in chars.clone() {
                let (ma, ca) = ours.rasterize(ch, px);
                let (mb, cb) = fd.rasterize(ch, px);
                assert!((ma.advance_width - mb.advance_width).abs() < 0.01, "{ch:?} advance");
                if mb.width == 0 || mb.height == 0 {
                    assert!(ca.iter().all(|&c| c == 0), "{ch:?} should be blank");
                    continue;
                }
                // Bounds may differ by a pixel at the edge (floor vs. ceil of
                // an exact-integer bound), so compare where the ink lands
                // rather than the boxes.
                assert!((ma.xmin - mb.xmin).abs() <= 1 && (ma.ymin - mb.ymin).abs() <= 1, "{ch:?} {ma:?} {mb:?}");
                let n = (px as usize) * 4;
                let place = |xmin: i32, ymin: i32, w: usize, h: usize, c: &[u8]| {
                    let mut out = vec![0i32; n * n];
                    for ry in 0..h {
                        for rx in 0..w {
                            let x = (n / 4) as i32 + xmin + rx as i32;
                            let y = (n / 2) as i32 - (ymin + h as i32) + ry as i32;
                            out[y as usize * n + x as usize] = c[ry * w + rx] as i32;
                        }
                    }
                    out
                };
                let a = place(ma.xmin, ma.ymin, ma.width, ma.height, &ca);
                let b = place(mb.xmin, mb.ymin, mb.width, mb.height, &cb);
                let diff: i32 = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum();
                let ink: i32 = b.iter().sum();
                // The two rasterizers round anti-aliased edges differently:
                // ~6% at worst (small punctuation at 12px), never a moved stroke.
                assert!(diff * 10 <= ink, "{ch:?} at {px}: {diff} differing coverage of {ink}");
            }
        }
    }

    #[test]
    fn missing_and_blank_glyphs_do_not_panic() {
        let Some((f, _)) = both() else { return };
        let (m, c) = f.rasterize(' ', 15.0);
        assert!(c.is_empty() && m.width == 0 && m.advance_width > 0.0);
        let _ = f.rasterize('\u{10FFFF}', 15.0); // notdef
        assert!(f.has_glyph('A') && !f.has_glyph('\u{10FFFF}'));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Font::from_bytes(vec![0; 16]).is_err());
    }

    #[test]
    fn a_mapped_font_draws_exactly_what_a_read_one_does() {
        let Ok(bytes) = std::fs::read(NERD) else { return }; // not installed (CI)
        let owned = Font::from_bytes(bytes).unwrap();
        let mapped = Font::from_path(NERD).unwrap();
        for c in ['A', 'g', '#', '█'] {
            assert_eq!(owned.rasterize(c, 21.0), mapped.rasterize(c, 21.0), "{c}");
            assert_eq!(owned.metrics(c, 21.0), mapped.metrics(c, 21.0));
        }
        assert!(Font::from_path("/nonexistent.ttf").is_err());
    }
}
