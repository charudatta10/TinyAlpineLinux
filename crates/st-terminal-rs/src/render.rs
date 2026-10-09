//! Software renderer: paints the VT screen into a softbuffer surface with
//! the embedded font8x8 font (scaled by nearest-neighbour sampling).

use std::collections::HashMap;

use font8x8::{
    BASIC_FONTS, BLOCK_FONTS, BOX_FONTS, GREEK_FONTS, HIRAGANA_FONTS, LATIN_FONTS, MISC_FONTS,
    SGA_FONTS, UnicodeFonts,
};

use crate::vt::{Color, Screen, FLAG_BOLD, FLAG_DIM, FLAG_INVERSE, FLAG_UNDERLINE};

pub struct Renderer {
    pub cell_w: u32,
    pub cell_h: u32,
    palette: [u32; 256],
    glyph_cache: HashMap<char, [u8; 8]>,
}

/// xterm's 16 ANSI base colors.
const BASE: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];

pub const DEFAULT_FG: u32 = 0xd0d0d0;
pub const DEFAULT_BG: u32 = 0x000000;

/// Cell size in pixels (8×16 doubles the 8×8 font vertically).
pub const CELL_W: u32 = 8;
pub const CELL_H: u32 = 16;

impl Renderer {
    pub fn new(cell_w: u32, cell_h: u32) -> Self {
        let mut palette = [0u32; 256];
        palette[..16].copy_from_slice(&BASE);
        // 6x6x6 colour cube.
        let levels = [0u8, 95, 135, 175, 215, 255];
        for i in 0..216 {
            let (r, gb) = (i / 36, i % 36);
            let (g, b) = (gb / 6, gb % 6);
            palette[16 + i] = pack3(levels[r], levels[g], levels[b]);
        }
        // Grayscale ramp.
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            palette[232 + i] = pack3(v, v, v);
        }
        Renderer {
            cell_w,
            cell_h,
            palette,
            glyph_cache: HashMap::new(),
        }
    }

    fn glyph(&mut self, c: char) -> [u8; 8] {
        if let Some(g) = self.glyph_cache.get(&c) {
            return *g;
        }
        let g = BASIC_FONTS
            .get(c)
            .or_else(|| BOX_FONTS.get(c))
            .or_else(|| BLOCK_FONTS.get(c))
            .or_else(|| MISC_FONTS.get(c))
            .or_else(|| LATIN_FONTS.get(c))
            .or_else(|| GREEK_FONTS.get(c))
            .or_else(|| SGA_FONTS.get(c))
            .or_else(|| HIRAGANA_FONTS.get(c))
            .unwrap_or([0; 8]);
        if self.glyph_cache.len() < 4096 {
            self.glyph_cache.insert(c, g);
        }
        g
    }

    fn resolve(&self, color: Color, bold: bool) -> u32 {
        match color {
            Color::Idx(i) => {
                let mut i = i as usize;
                if bold && i < 8 {
                    i += 8;
                }
                self.palette[i.min(255)]
            }
            Color::Rgb(r, g, b) => pack3(r, g, b),
        }
    }

    /// Render the current screen view into a raw pixel buffer.
    pub fn render(&mut self, buf: &mut [u32], w: usize, h: usize, screen: &Screen) {
        let (cols, rows) = (screen.cols, screen.rows);
        let (cw, ch) = (self.cell_w as usize, self.cell_h as usize);

        if w == 0 || h == 0 || buf.len() < w * h {
            return;
        }

        // Cursor position: only on the live view.
        let cursor = if screen.scroll_offset == 0 && screen.cursor_visible {
            Some((screen.x, screen.y))
        } else {
            None
        };

        for row in 0..rows.min((h + ch - 1) / ch) {
            let Some(line) = screen.viewport_line(row) else {
                // Blank row.
                for y in row * ch..((row + 1) * ch).min(h) {
                    let start = y * w;
                    let end = ((row + 1) * ch).min(h) * w;
                    buf[start..end].fill(DEFAULT_BG);
                }
                continue;
            };
            for col in 0..cols.min((w + cw - 1) / cw) {
                let cell = line[col];
                let mut fg = self.resolve(cell.attrs.fg, cell.attrs.flags & FLAG_BOLD != 0);
                let mut bg = self.resolve(cell.attrs.bg, false);
                if cell.attrs.flags & FLAG_DIM != 0 {
                    fg = blend(fg, bg);
                }
                if cell.attrs.flags & FLAG_INVERSE != 0 {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let is_cursor = cursor == Some((col, row));
                if is_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bits = self.glyph(cell.ch);
                let underline = cell.attrs.flags & FLAG_UNDERLINE != 0;

                for py in 0..ch {
                    let fy = py * 8 / ch;
                    let row_bits = bits[fy];
                    let screen_y = row * ch + py;
                    if screen_y >= h {
                        break;
                    }
                    let underline_row = underline && py + 1 == ch;
                    for px in 0..cw {
                        let fx = px * 8 / cw;
                        let on = row_bits >> fx & 1 == 1;
                        let screen_x = col * cw + px;
                        if screen_x >= w {
                            break;
                        }
                        buf[screen_y * w + screen_x] = if on || underline_row { fg } else { bg };
                    }
                }
            }
        }
    }
}

fn pack3(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// 50% blend `fg` toward `bg` (for DIM).
fn blend(fg: u32, bg: u32) -> u32 {
    let (fr, fg_, fb) = (fg >> 16 & 0xff, fg >> 8 & 0xff, fg & 0xff);
    let (br, bg_, bb) = (bg >> 16 & 0xff, bg >> 8 & 0xff, bg & 0xff);
    pack3(((fr + br) / 2) as u8, ((fg_ + bg_) / 2) as u8, ((fb + bb) / 2) as u8)
}

