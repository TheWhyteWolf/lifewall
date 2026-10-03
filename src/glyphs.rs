// SPDX-License-Identifier: GPL-3.0-or-later
// Cell coverage for --layer: every glyph the board can draw, pre-rasterized
// into a cell-sized coverage bitmap and numbered, so the GPU can look a cell's
// glyph up by index. The charset is fixed at startup (--char), so the fonts
// are only needed while the atlas is built and are dropped afterwards.
//
// Fonts come from fontconfig, like kitty's: the family for the grid, then a
// per-glyph fallback for anything it lacks (kana mode needs a CJK font).

use lifefont::Font;
use std::collections::HashMap;
use std::process::Command;

/// The GPU indexes glyphs with one byte; the last index is the blank cell.
pub const MAX_GLYPHS: usize = 255;

pub struct CellAtlas {
    pub cw: usize,
    pub ch: usize,
    cells: HashMap<char, Vec<u8>>,
    blank: Vec<u8>,
    order: Vec<char>,
    index: HashMap<char, u8>,
    ascii: [u8; 128],
}

impl CellAtlas {
    /// cw*ch coverage bytes, row-major. Unknown glyphs are blank.
    pub fn cell(&self, c: char) -> &[u8] {
        self.cells.get(&c).unwrap_or(&self.blank)
    }

    /// Number of slots: every glyph plus the trailing blank.
    pub fn slots(&self) -> usize {
        self.order.len() + 1
    }

    /// Slot of `c` (the blank slot if it isn't in the atlas). Called for every
    /// live cell every frame, so ASCII skips the hash.
    pub fn index(&self, c: char) -> u8 {
        let blank = self.order.len() as u8;
        match c as u32 {
            n if n < 128 => self.ascii[n as usize],
            _ => self.index.get(&c).copied().unwrap_or(blank),
        }
    }

    /// Coverage for slot `i` (glyph order, then blank), for packing a texture.
    pub fn slot(&self, i: usize) -> &[u8] {
        self.order.get(i).map_or(&self.blank, |&c| self.cell(c))
    }
}

/// `fc-match` a pattern to (file, face index).
fn fc_match(pattern: &str) -> Option<(String, u32)> {
    let out = Command::new("fc-match").args(["-f", "%{file}\n%{index}", pattern]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let mut it = s.lines();
    let file = it.next().filter(|f| !f.is_empty())?.to_string();
    let index = it.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
    Some((file, index))
}

fn load(found: Option<(String, u32)>) -> Option<Font> {
    let (file, index) = found?;
    Font::from_path_index(&file, index).map_err(|e| eprintln!("lifewall: {e}")).ok()
}

/// Build the atlas for one output scale. `pt` is kitty-style points (96 dpi).
pub fn build(family: &str, pt: f64, scale: i32, glyphs: &[char]) -> Result<CellAtlas, String> {
    let px = (pt * 96.0 / 72.0 * scale as f64) as f32;
    let primary = load(fc_match(family)).ok_or_else(|| format!("no font for family '{family}'"))?;
    let lm = primary.horizontal_line_metrics(px).ok_or("font has no line metrics")?;
    let cw = primary.metrics('0', px).advance_width.round().max(1.0) as usize;
    let ch = (lm.ascent - lm.descent + lm.line_gap).round().max(1.0) as usize;
    // Baseline: the line gap is split above and below, as terminals do.
    let baseline = (lm.line_gap / 2.0 + lm.ascent).round() as i32;

    let mut fallbacks: Vec<(String, Font)> = Vec::new();
    let mut cells = HashMap::new();
    let mut order = Vec::new();
    for &c in glyphs {
        if cells.contains_key(&c) {
            continue;
        }
        if order.len() == MAX_GLYPHS - 1 {
            eprintln!("lifewall: more than {} distinct glyphs; the rest draw blank", MAX_GLYPHS - 1);
            break;
        }
        order.push(c);
        let font = if primary.has_glyph(c) {
            &primary
        } else {
            match fallback_for(c, &mut fallbacks) {
                Some(f) => f,
                None => &primary, // draws .notdef: visible, and says what's wrong
            }
        };
        cells.insert(c, rasterize(font, c, px, cw, ch, baseline));
    }
    let index: HashMap<char, u8> = order.iter().enumerate().map(|(i, &c)| (c, i as u8)).collect();
    let mut ascii = [order.len() as u8; 128];
    for (&c, &i) in &index {
        if (c as u32) < 128 {
            ascii[c as usize] = i;
        }
    }
    Ok(CellAtlas { cw, ch, cells, blank: vec![0; cw * ch], order, index, ascii })
}

/// A font that has `c`, reusing one already loaded when it covers it.
fn fallback_for(c: char, loaded: &mut Vec<(String, Font)>) -> Option<&Font> {
    if let Some(i) = loaded.iter().position(|(_, f)| f.has_glyph(c)) {
        return Some(&loaded[i].1);
    }
    let found = fc_match(&format!(":charset={:x}", c as u32))?;
    let key = format!("{}#{}", found.0, found.1);
    if loaded.iter().any(|(k, _)| *k == key) {
        return None; // fontconfig's best match is a font we know lacks it
    }
    let font = load(Some(found))?;
    if !font.has_glyph(c) {
        return None;
    }
    loaded.push((key, font));
    loaded.last().map(|(_, f)| f)
}

/// One glyph into a cw*ch cell: baseline-true, centred on its advance, and
/// shrunk to fit when it is wider than the cell (CJK is double width).
fn rasterize(font: &Font, c: char, px: f32, cw: usize, ch: usize, baseline: i32) -> Vec<u8> {
    let adv = font.metrics(c, px).advance_width;
    let px = if adv > cw as f32 * 1.05 { px * cw as f32 / adv } else { px };
    let (m, cov) = font.rasterize(c, px);
    let mut out = vec![0u8; cw * ch];
    let x0 = m.xmin + ((cw as f32 - m.advance_width) / 2.0).round() as i32;
    let y0 = baseline - (m.ymin + m.height as i32);
    for ry in 0..m.height {
        let y = y0 + ry as i32;
        if y < 0 || y as usize >= ch {
            continue;
        }
        for rx in 0..m.width {
            let x = x0 + rx as i32;
            if x < 0 || x as usize >= cw {
                continue;
            }
            out[y as usize * cw + x as usize] = cov[ry * m.width + rx];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs fontconfig and the rice font; skips cleanly where they are absent.
    fn atlas(glyphs: &str) -> Option<CellAtlas> {
        fc_match("ShureTechMono Nerd Font")?;
        build("ShureTechMono Nerd Font", 8.0, 1, &glyphs.chars().collect::<Vec<_>>()).ok()
    }

    #[test]
    fn cells_are_cell_sized_and_inked() {
        let Some(a) = atlas("#A") else { return };
        assert!(a.cw >= 4 && a.ch > a.cw, "{}x{}", a.cw, a.ch);
        for c in ['#', 'A'] {
            let cell = a.cell(c);
            assert_eq!(cell.len(), a.cw * a.ch);
            assert!(cell.iter().any(|&v| v > 128), "{c} has no ink");
        }
        assert!(a.cell('z').iter().all(|&v| v == 0), "not in the charset: blank");
        assert_eq!((a.index('#'), a.index('A'), a.index('z')), (0, 1, 2), "unknown -> blank slot");
        assert_eq!(a.slots(), 3);
    }

    #[test]
    fn scale_two_doubles_the_cell() {
        let Some(one) = atlas("#") else { return };
        let two = build("ShureTechMono Nerd Font", 8.0, 2, &['#']).unwrap();
        assert!(two.cw.abs_diff(one.cw * 2) <= 1 && two.ch.abs_diff(one.ch * 2) <= 1);
    }

    #[test]
    fn kana_falls_back_to_a_font_that_has_it() {
        // Only meaningful where a CJK font is installed (noto-fonts-cjk).
        if fc_match(":charset=3042").is_none() {
            return;
        }
        let Some(a) = atlas("あ") else { return };
        assert!(a.cell('あ').iter().filter(|&&v| v > 128).count() > 4, "kana drew nothing");
    }
}
