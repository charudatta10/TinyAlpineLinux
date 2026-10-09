//! VT100/xterm-compatible terminal emulator core: a screen buffer with
//! scrollback plus a byte-oriented escape-sequence parser.
//!
//! `Screen::feed` consumes output bytes from the child process and returns
//! any response sequences that must be written back (DSR, DA, ...).

use std::collections::VecDeque;

// ---------------------------------------------------------------- colors

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Idx(u8),
    Rgb(u8, u8, u8),
}

pub const FLAG_BOLD: u8 = 1 << 0;
pub const FLAG_DIM: u8 = 1 << 1;
pub const FLAG_UNDERLINE: u8 = 1 << 2;
pub const FLAG_INVERSE: u8 = 1 << 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Attrs {
    fn default() -> Self {
        Attrs {
            fg: Color::Idx(7),
            bg: Color::Idx(0),
            flags: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            attrs: Attrs::default(),
        }
    }
}

// ---------------------------------------------------------------- parser state

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Dcs,
    DcsEsc,
    EscSwallow,
    Utf8(u8, u32), // (remaining continuation bytes, accumulated value)
}

#[derive(Clone)]
struct Saved {
    x: usize,
    y: usize,
    attrs: Attrs,
    origin: bool,
}

pub const DEFAULT_SCROLLBACK: usize = 5000;

// ---------------------------------------------------------------- screen

pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    main: Vec<Cell>,
    alt: Vec<Cell>,
    pub alt_active: bool,
    pub scrollback: VecDeque<Vec<Cell>>,
    pub scrollback_limit: usize,
    /// How many lines the viewport is scrolled back (0 = live view).
    pub scroll_offset: usize,

    pub x: usize,
    pub y: usize,
    attrs: Attrs,
    top: usize,
    bot: usize,
    wrap_pending: bool,
    saved_main: Saved,
    saved_alt: Saved,

    pub cursor_visible: bool,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    pub autowrap: bool,
    mouse_mode: u16,
    pub title: String,
    pub title_changed: bool,
    /// A BEL was received (bell indicator).
    pub bell: bool,

    // parser internals
    state: State,
    csi_params: Vec<u16>,
    csi_cur: Option<u16>,
    csi_private: Option<char>,
    osc_buf: String,
}

impl Screen {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let n = cols * rows;
        Screen {
            cols,
            rows,
            main: vec![Cell::default(); n],
            alt: vec![Cell::default(); n],
            alt_active: false,
            scrollback: VecDeque::new(),
            scrollback_limit: scrollback.max(1),
            scroll_offset: 0,
            x: 0,
            y: 0,
            attrs: Attrs::default(),
            top: 0,
            bot: rows - 1,
            wrap_pending: false,
            saved_main: Saved {
                x: 0,
                y: 0,
                attrs: Attrs::default(),
                origin: false,
            },
            saved_alt: Saved {
                x: 0,
                y: 0,
                attrs: Attrs::default(),
                origin: false,
            },
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            autowrap: true,
            mouse_mode: 0,
            title: String::new(),
            title_changed: false,
            bell: false,
            state: State::Ground,
            csi_params: Vec::new(),
            csi_cur: None,
            csi_private: None,
            osc_buf: String::new(),
        }
    }

    fn grid(&self) -> &[Cell] {
        if self.alt_active {
            &self.alt
        } else {
            &self.main
        }
    }

    fn grid_mut(&mut self) -> &mut Vec<Cell> {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    /// The visible viewport line `i` (0..rows): history first, live screen
    /// after. Returns `None` for scrolled-past rows that have no content.
    pub fn viewport_line(&self, i: usize) -> Option<&[Cell]> {
        if self.scroll_offset == 0 {
            if i < self.rows {
                let s = self.idx(0, i);
                return Some(&self.grid()[s..s + self.cols]);
            }
            return None;
        }
        let sb = &self.scrollback;
        // global index: history entries first, then live rows.
        let total_history = sb.len();
        let g = total_history - self.scroll_offset.min(total_history) + i;
        if g < total_history {
            Some(&sb[g])
        } else {
            let live = g - total_history;
            if live < self.rows {
                let s = self.idx(0, live);
                Some(&self.grid()[s..s + self.cols])
            } else {
                None
            }
        }
    }

    pub fn max_scroll(&self) -> usize {
        self.scrollback.len()
    }

    /// Scroll the viewport by `delta` lines (positive = older).
    pub fn scroll_view(&mut self, delta: isize) {
        let max = self.scrollback.len();
        let cur = self.scroll_offset as isize;
        let next = (cur + delta).clamp(0, max as isize);
        self.scroll_offset = next as usize;
    }

    /// New output: snap back to the live view.
    pub fn reset_view(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut new_main = vec![Cell::default(); cols * rows];
        for y in 0..self.rows.min(rows) {
            for x in 0..self.cols.min(cols) {
                new_main[y * cols + x] = self.main[y * self.cols + x];
            }
        }
        let mut new_alt = vec![Cell::default(); cols * rows];
        for y in 0..self.rows.min(rows) {
            for x in 0..self.cols.min(cols) {
                new_alt[y * cols + x] = self.alt[y * self.cols + x];
            }
        }
        self.main = new_main;
        self.alt = new_alt;
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.x = self.x.min(cols - 1);
        self.y = self.y.min(rows - 1);
        self.wrap_pending = false;
        self.scroll_offset = self.scroll_offset.min(self.scrollback.len());
    }

    // ------------------------------------------------------------ feed

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut reply = Vec::new();
        for &b in bytes {
            self.byte(b, &mut reply);
        }
        reply
    }

    fn byte(&mut self, b: u8, reply: &mut Vec<u8>) {
        match self.state {
            State::Ground => self.ground(b, reply),
            State::Esc => self.esc(b),
            State::Csi => self.csi_byte(b, reply),
            State::Osc => {
                if b == 0x07 {
                    self.state = State::Ground;
                    self.osc_dispatch();
                } else if b == 0x1b {
                    self.state = State::OscEsc;
                } else {
                    self.osc_buf.push(b as char);
                    if self.osc_buf.len() > 4096 {
                        self.state = State::Ground;
                    }
                }
            }
            State::OscEsc => {
                if b == b'\\' {
                    self.state = State::Ground;
                    self.osc_dispatch();
                } else {
                    self.osc_buf.push(0x1b as char);
                    self.osc_buf.push(b as char);
                    self.state = State::Osc;
                }
            }
            State::EscSwallow => {
                self.state = State::Ground;
            }
            State::Dcs => {
                if b == 0x1b {
                    self.state = State::DcsEsc;
                } else if (0x40..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::DcsEsc => {
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Dcs;
                }
            }
            State::Utf8(rem, val) => {
                if b & 0xc0 == 0x80 {
                    let val = (val << 6) | (b & 0x3f) as u32;
                    if rem == 1 {
                        self.state = State::Ground;
                        if let Some(c) = char::from_u32(val) {
                            self.put_char(c);
                        }
                    } else {
                        self.state = State::Utf8(rem - 1, val);
                    }
                } else {
                    // Invalid continuation: restart in Ground.
                    self.state = State::Ground;
                    self.ground(b, reply);
                }
            }
        }
    }

    fn ground(&mut self, b: u8, reply: &mut Vec<u8>) {
        match b {
            0x1b => self.state = State::Esc,
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            b'\r' => {
                self.x = 0;
                self.wrap_pending = false;
            }
            0x08 => {
                self.x = self.x.saturating_sub(1);
                self.wrap_pending = false;
            }
            b'\t' => {
                let nx = ((self.x / 8) + 1) * 8;
                self.x = nx.min(self.cols.saturating_sub(1));
            }
            0x07 => self.bell = true,
            0x00..=0x06 | 0x0e..=0x1a | 0x1c..=0x1f => {}
            b if b < 0x80 => self.put_char(b as char),
            0xc0..=0xdf => self.state = State::Utf8(1, (b & 0x1f) as u32),
            0xe0..=0xef => self.state = State::Utf8(2, (b & 0x0f) as u32),
            0xf0..=0xf7 => self.state = State::Utf8(3, (b & 0x07) as u32),
            _ => {
                let _ = reply;
            }
        }
    }

    fn esc(&mut self, b: u8) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.state = State::Csi;
                self.csi_params.clear();
                self.csi_cur = None;
                self.csi_private = None;
            }
            b']' => {
                self.state = State::Osc;
                self.osc_buf.clear();
            }
            b'P' => self.state = State::Dcs,
            b'7' => {
                self.saved_main = Saved {
                    x: self.x,
                    y: self.y,
                    attrs: self.attrs,
                    origin: false,
                };
            }
            b'8' => {
                let s = if self.alt_active {
                    self.saved_alt.clone()
                } else {
                    self.saved_main.clone()
                };
                self.x = s.x.min(self.cols - 1);
                self.y = s.y.min(self.rows - 1);
                self.attrs = s.attrs;
            }
            b'D' => self.line_feed(),
            b'E' => {
                self.x = 0;
                self.line_feed();
            }
            b'H' => {} // HTS: tab stops ignored (fixed 8)
            b'M' => self.reverse_index(),
            b'c' => self.full_reset(),
            b'(' | b')' | b'#' | b'%' => self.state = State::EscSwallow,
            b'=' | b'>' => {} // keypad modes
            _ => {}
        }
    }

    fn csi_byte(&mut self, b: u8, reply: &mut Vec<u8>) {
        match b {
            b'0'..=b'9' => {
                let d = (b - b'0') as u16;
                self.csi_cur = Some(self.csi_cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
            }
            b';' | b':' => {
                let v = self.csi_cur.take().unwrap_or(0);
                self.csi_params.push(v);
            }
            0x3c..=0x3f if self.csi_params.is_empty() && self.csi_cur.is_none() => {
                // '<=>?' private parameter prefix
                self.csi_private = Some(b as char);
            }
            0x20..=0x2f => {} // intermediate bytes (e.g. DECSTR) ignored
            0x40..=0x7e => {
                if let Some(v) = self.csi_cur.take() {
                    self.csi_params.push(v);
                }
                self.state = State::Ground;
                self.csi_dispatch(b as char, reply);
            }
            0x1b => {
                self.state = State::Esc;
            }
            0x07 => {
                self.state = State::Ground;
                self.bell = true;
            }
            _ => {
                self.state = State::Ground;
            }
        }
    }

    fn osc_dispatch(&mut self) {
        let buf = std::mem::take(&mut self.osc_buf);
        if let Some((num, rest)) = buf.split_once(';') {
            if num == "0" || num == "2" {
                self.title = rest.to_string();
                self.title_changed = true;
            }
        }
    }

    // ------------------------------------------------------------ params

    fn p(&self, i: usize, default: u16) -> u16 {
        match self.csi_params.get(i) {
            Some(0) | None => default,
            Some(v) => *v,
        }
    }

    fn p0(&self, i: usize) -> u16 {
        self.csi_params.get(i).copied().unwrap_or(0)
    }

    // ------------------------------------------------------------ dispatch

    fn csi_dispatch(&mut self, final_byte: char, reply: &mut Vec<u8>) {
        let private = self.csi_private;
        if private == Some('?') {
            match final_byte {
                'h' => self.set_modes(true),
                'l' => self.set_modes(false),
                'J' | 'K' => {}
                _ => {}
            }
            return;
        }
        match final_byte {
            'A' => self.move_cursor(0, -(self.p(0, 1) as i32)),
            'B' | 'e' => self.move_cursor(0, self.p(0, 1) as i32),
            'C' | 'a' => self.move_cursor(self.p(0, 1) as i32, 0),
            'D' => self.move_cursor(-(self.p(0, 1) as i32), 0),
            'E' => {
                self.move_cursor(0, self.p(0, 1) as i32);
                self.x = 0;
            }
            'F' => {
                self.move_cursor(0, -(self.p(0, 1) as i32));
                self.x = 0;
            }
            'G' | '`' => {
                self.x = (self.p(0, 1) as usize - 1).min(self.cols - 1);
                self.wrap_pending = false;
            }
            'H' | 'f' => {
                let y = (self.p(0, 1) as usize - 1) as i32;
                let x = (self.p(1, 1) as usize - 1) as i32;
                self.goto(x, y);
            }
            'd' => {
                let y = (self.p(0, 1) as usize - 1) as i32;
                self.goto(self.x as i32, y);
            }
            'I' => {
                for _ in 0..self.p(0, 1) {
                    let nx = ((self.x / 8) + 1) * 8;
                    self.x = nx.min(self.cols.saturating_sub(1));
                }
            }
            'Z' => {
                // CBT: back tab
                self.x = (self.x.saturating_sub(8) / 8) * 8;
            }
            'J' => self.erase_display(self.p0(0)),
            'K' => self.erase_line(self.p0(0)),
            'L' => self.insert_lines(self.p(0, 1) as usize),
            'M' => self.delete_lines(self.p(0, 1) as usize),
            'P' => self.delete_chars(self.p(0, 1) as usize),
            '@' => self.insert_chars(self.p(0, 1) as usize),
            'X' => self.erase_chars(self.p(0, 1) as usize),
            'S' => self.scroll_region_up(self.p(0, 1) as usize),
            'T' => self.scroll_region_down(self.p(0, 1) as usize),
            'm' => self.sgr(),
            'r' => {
                let top = (self.p(0, 1) as usize - 1).min(self.rows - 1);
                let bot = (self.p(1, self.rows as u16) as usize - 1).min(self.rows - 1);
                if top < bot {
                    self.top = top;
                    self.bot = bot;
                    self.goto(0, top as i32);
                }
            }
            's' => {
                self.saved_main = Saved {
                    x: self.x,
                    y: self.y,
                    attrs: self.attrs,
                    origin: false,
                };
            }
            'u' => {
                self.x = self.saved_main.x.min(self.cols - 1);
                self.y = self.saved_main.y.min(self.rows - 1);
            }
            'h' => self.set_modes(true),
            'l' => self.set_modes(false),
            'g' => {} // tab stop clear: ignored
            'c' => reply.extend_from_slice(b"\x1b[?6c"), // DA1: VT102
            'n' => match self.p0(0) {
                5 => reply.extend_from_slice(b"\x1b[0n"),
                6 => {
                    reply.extend_from_slice(format!("\x1b[{};{}R", self.y + 1, self.x + 1).as_bytes());
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// `CSI ? n h/l` (private) and `CSI n h/l` (non-private, mostly ignored).
    fn set_modes(&mut self, set: bool) {
        if self.csi_private != Some('?') {
            return;
        }
        let params: Vec<u16> = self.csi_params.clone();
        for p in params {
            match p {
                1 => self.app_cursor_keys = set,
                6 => {} // origin mode: approximated by absolute positioning
                7 => self.autowrap = set,
                25 => self.cursor_visible = set,
                47 | 1047 => self.switch_alt(set, false),
                1048 => {
                    if set {
                        self.saved_main = Saved {
                            x: self.x,
                            y: self.y,
                            attrs: self.attrs,
                            origin: false,
                        };
                    } else {
                        self.x = self.saved_main.x.min(self.cols - 1);
                        self.y = self.saved_main.y.min(self.rows - 1);
                    }
                }
                1049 => self.switch_alt(set, true),
                9 | 1002 | 1003 | 1005 | 1015 | 1000..=1006 => {
                    if set {
                        self.mouse_mode = p;
                    } else {
                        self.mouse_mode = 0;
                    }
                }
                2004 => self.bracketed_paste = set,
                _ => {}
            }
        }
    }

    fn switch_alt(&mut self, on: bool, save_cursor: bool) {
        if on && !self.alt_active {
            if save_cursor {
                self.saved_main = Saved {
                    x: self.x,
                    y: self.y,
                    attrs: self.attrs,
                    origin: false,
                };
            }
            self.alt_active = true;
            self.alt.iter_mut().for_each(|c| *c = Cell::default());
            self.x = 0;
            self.y = 0;
            self.scroll_offset = 0;
        } else if !on && self.alt_active {
            self.alt_active = false;
            if save_cursor {
                self.x = self.saved_main.x.min(self.cols - 1);
                self.y = self.saved_main.y.min(self.rows - 1);
                self.attrs = self.saved_main.attrs;
            }
        }
    }

    fn sgr(&mut self) {
        if self.csi_params.is_empty() {
            self.attrs = Attrs::default();
            return;
        }
        let mut i = 0;
        while i < self.csi_params.len() {
            let p = self.csi_params[i];
            match p {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.flags |= FLAG_BOLD,
                2 => self.attrs.flags |= FLAG_DIM,
                3 => {}
                4 => self.attrs.flags |= FLAG_UNDERLINE,
                7 => self.attrs.flags |= FLAG_INVERSE,
                21 | 22 => self.attrs.flags &= !(FLAG_BOLD | FLAG_DIM),
                23 => {}
                24 => self.attrs.flags &= !FLAG_UNDERLINE,
                27 => self.attrs.flags &= !FLAG_INVERSE,
                30..=37 => self.attrs.fg = Color::Idx((p - 30) as u8),
                38 => {
                    if let Some((color, used)) = parse_ext_color(&self.csi_params[i..]) {
                        self.attrs.fg = color;
                        i += used - 1;
                    } else {
                        i = self.csi_params.len();
                    }
                }
                39 => self.attrs.fg = Attrs::default().fg,
                40..=47 => self.attrs.bg = Color::Idx((p - 40) as u8),
                48 => {
                    if let Some((color, used)) = parse_ext_color(&self.csi_params[i..]) {
                        self.attrs.bg = color;
                        i += used - 1;
                    } else {
                        i = self.csi_params.len();
                    }
                }
                49 => self.attrs.bg = Attrs::default().bg,
                90..=97 => self.attrs.fg = Color::Idx((p - 90 + 8) as u8),
                100..=107 => self.attrs.bg = Color::Idx((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    // ------------------------------------------------------------ ops

    fn put_char(&mut self, c: char) {
        if self.wrap_pending && self.autowrap {
            self.x = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        let idx = self.idx(self.x, self.y);
        let cell = Cell {
            ch: c,
            attrs: self.attrs,
        };
        self.grid_mut()[idx] = cell;
        if self.x + 1 >= self.cols {
            if self.autowrap {
                self.wrap_pending = true;
            }
        } else {
            self.x += 1;
        }
    }

    fn line_feed(&mut self) {
        if self.y == self.bot {
            self.scroll_region_up(1);
        } else if self.y + 1 < self.rows {
            self.y += 1;
        }
        self.wrap_pending = false;
    }

    fn reverse_index(&mut self) {
        if self.y == self.top {
            self.scroll_region_down(1);
        } else if self.y > 0 {
            self.y -= 1;
        }
        self.wrap_pending = false;
    }

    fn move_cursor(&mut self, dx: i32, dy: i32) {
        let nx = (self.x as i32 + dx).clamp(0, self.cols as i32 - 1);
        let ny = (self.y as i32 + dy).clamp(0, self.rows as i32 - 1);
        self.x = nx as usize;
        self.y = ny as usize;
        self.wrap_pending = false;
    }

    fn goto(&mut self, x: i32, y: i32) {
        self.x = x.clamp(0, self.cols as i32 - 1) as usize;
        self.y = y.clamp(0, self.rows as i32 - 1) as usize;
        self.wrap_pending = false;
    }

    fn clear_line_range(&mut self, row: usize, from: usize, to: usize, attrs: Attrs) {
        let cols = self.cols;
        let grid = self.grid_mut();
        for x in from..=to.min(cols - 1) {
            grid[row * cols + x] = Cell { ch: ' ', attrs };
        }
    }

    fn scroll_region_up(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        if n == 0 {
            return;
        }
        // Push lines into scrollback only when viewing the full main screen.
        if !self.alt_active && self.top == 0 && self.bot == self.rows - 1 {
            for i in 0..n {
                let line: Vec<Cell> = self.main[i * self.cols..(i + 1) * self.cols].to_vec();
                self.scrollback.push_back(line);
            }
            while self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
            }
        }
        let cols = self.cols;
        let top = self.top;
        let bot = self.bot;
        let grid = self.grid_mut();
        for row in top..bot + 1 - n {
            for x in 0..cols {
                grid[row * cols + x] = grid[(row + n) * cols + x];
            }
        }
        for row in bot + 1 - n..=bot {
            for x in 0..cols {
                grid[row * cols + x] = Cell::default();
            }
        }
    }

    fn scroll_region_down(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        if n == 0 {
            return;
        }
        let cols = self.cols;
        let top = self.top;
        let bot = self.bot;
        let grid = self.grid_mut();
        let mut row = bot + 1;
        while row > top + n {
            row -= 1;
            for x in 0..cols {
                grid[row * cols + x] = grid[(row - n) * cols + x];
            }
        }
        for row in top..top + n {
            for x in 0..cols {
                grid[row * cols + x] = Cell::default();
            }
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let attrs = self.attrs;
        match mode {
            0 => {
                self.clear_line_range(self.y, self.x, self.cols - 1, attrs);
                for y in self.y + 1..self.rows {
                    self.clear_line_range(y, 0, self.cols - 1, attrs);
                }
            }
            1 => {
                for y in 0..self.y {
                    self.clear_line_range(y, 0, self.cols - 1, attrs);
                }
                self.clear_line_range(self.y, 0, self.x, attrs);
            }
            2 => {
                for y in 0..self.rows {
                    self.clear_line_range(y, 0, self.cols - 1, attrs);
                }
            }
            3 => {
                self.scrollback.clear();
                self.scroll_offset = 0;
            }
            _ => {}
        }
        self.wrap_pending = false;
    }

    fn erase_line(&mut self, mode: u16) {
        let attrs = self.attrs;
        match mode {
            0 => self.clear_line_range(self.y, self.x, self.cols - 1, attrs),
            1 => self.clear_line_range(self.y, 0, self.x, attrs),
            2 => self.clear_line_range(self.y, 0, self.cols - 1, attrs),
            _ => {}
        }
        self.wrap_pending = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let n = n.min(self.cols - self.x);
        let cols = self.cols;
        let attrs = self.attrs;
        let row = self.y;
        let x = self.x;
        let grid = self.grid_mut();
        let base = row * cols;
        for i in (x + n..cols).rev() {
            grid[base + i] = grid[base + i - n];
        }
        for i in x..x + n {
            grid[base + i] = Cell { ch: ' ', attrs };
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let n = n.min(self.cols - self.x);
        let cols = self.cols;
        let attrs = self.attrs;
        let row = self.y;
        let x = self.x;
        let grid = self.grid_mut();
        let base = row * cols;
        for i in x..cols - n {
            grid[base + i] = grid[base + i + n];
        }
        for i in cols - n..cols {
            grid[base + i] = Cell { ch: ' ', attrs };
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let n = n.min(self.cols - self.x);
        let attrs = self.attrs;
        self.clear_line_range(self.y, self.x, self.x + n - 1, attrs);
    }

    fn insert_lines(&mut self, n: usize) {
        if self.y < self.top || self.y > self.bot {
            return;
        }
        let saved_top = self.top;
        self.top = self.y;
        self.bot = self.bot;
        // Shift down within [y, bot]
        let n = n.min(self.bot - self.y + 1);
        let cols = self.cols;
        let y = self.y;
        let bot = self.bot;
        let grid = self.grid_mut();
        let mut row = bot + 1;
        while row > y + n {
            row -= 1;
            for x in 0..cols {
                grid[row * cols + x] = grid[(row - n) * cols + x];
            }
        }
        for row in y..y + n {
            for x in 0..cols {
                grid[row * cols + x] = Cell::default();
            }
        }
        self.top = saved_top;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.y < self.top || self.y > self.bot {
            return;
        }
        let n = n.min(self.bot - self.y + 1);
        let cols = self.cols;
        let y = self.y;
        let bot = self.bot;
        let grid = self.grid_mut();
        for row in y..bot + 1 - n {
            for x in 0..cols {
                grid[row * cols + x] = grid[(row + n) * cols + x];
            }
        }
        for row in bot + 1 - n..=bot {
            for x in 0..cols {
                grid[row * cols + x] = Cell::default();
            }
        }
    }

    fn full_reset(&mut self) {
        self.main.iter_mut().for_each(|c| *c = Cell::default());
        self.alt.iter_mut().for_each(|c| *c = Cell::default());
        self.attrs = Attrs::default();
        self.x = 0;
        self.y = 0;
        self.top = 0;
        self.bot = self.rows - 1;
        self.cursor_visible = true;
        self.wrap_pending = false;
        self.state = State::Ground;
    }

    pub fn mouse_reporting(&self) -> u16 {
        self.mouse_mode
    }
}

fn parse_ext_color(params: &[u16]) -> Option<(Color, usize)> {
    match params.get(1)? {
        5 => {
            let idx = *params.get(2)?;
            Some((Color::Idx(idx.min(255) as u8), 3))
        }
        2 => {
            let r = *params.get(2)? as u8;
            let g = *params.get(3)? as u8;
            let b = *params.get(4)? as u8;
            Some((Color::Rgb(r, g, b), 5))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen {
        Screen::new(20, 5, 100)
    }

    fn text(s: &Screen) -> Vec<String> {
        (0..s.rows)
            .filter_map(|y| s.viewport_line(y))
            .map(|line| line.iter().map(|c| c.ch).collect::<String>())
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    #[test]
    fn prints_plain_text() {
        let mut s = screen();
        s.feed(b"hello");
        assert_eq!(text(&s)[0], "hello");
        assert_eq!(s.x, 5);
    }

    #[test]
    fn cr_lf_and_backspace() {
        let mut s = screen();
        s.feed(b"abc\r\ndef");
        assert_eq!(text(&s)[0], "abc");
        assert_eq!(text(&s)[1], "def");
        let mut s = screen();
        s.feed(b"abc\x08X");
        assert_eq!(text(&s)[0], "abX");
    }

    #[test]
    fn tabs() {
        let mut s = screen();
        s.feed(b"a\tb");
        assert_eq!(s.x, 9);
        assert_eq!(text(&s)[0].chars().nth(8), Some('b'));
    }

    #[test]
    fn cursor_positioning() {
        let mut s = screen();
        s.feed(b"\x1b[3;5HX");
        assert_eq!(text(&s)[2].chars().nth(4), Some('X'));
        s.feed(b"\x1b[2A");
        assert_eq!(s.y, 0);
        s.feed(b"\x1b[3D");
        // cursor was at x=5 after the X advanced it
        assert_eq!(s.x, 2);
    }

    #[test]
    fn erase_line_and_display() {
        let mut s = screen();
        s.feed(b"abcdef\x1b[3D\x1b[K");
        assert_eq!(text(&s)[0], "abc");
        s.feed(b"\x1b[2J");
        assert!(text(&s).iter().all(|l| l.is_empty()));
    }

    #[test]
    fn sgr_colors_roundtrip() {
        let mut s = screen();
        s.feed(b"\x1b[31mred\x1b[0m plain");
        let cell = s.viewport_line(0).unwrap()[0];
        assert_eq!(cell.attrs.fg, Color::Idx(1));
        let cell = s.viewport_line(0).unwrap()[8];
        assert_eq!(cell.attrs.fg, Color::Idx(7));
        assert_eq!(cell.attrs.flags, 0);
    }

    #[test]
    fn sgr_256_and_truecolor() {
        let mut s = screen();
        s.feed(b"\x1b[38;5;123mA\x1b[48;2;10;20;30mB");
        let a = s.viewport_line(0).unwrap()[0];
        assert_eq!(a.attrs.fg, Color::Idx(123));
        let b = s.viewport_line(0).unwrap()[1];
        assert_eq!(b.attrs.bg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn scrolling_into_history() {
        let mut s = screen();
        for i in 0..10 {
            s.feed(format!("line{i}\r\n").as_bytes());
        }
        // The trailing \r\n after line9 scrolls once more.
        assert_eq!(s.scrollback.len(), 6);
        // view is live by default
        assert_eq!(s.scroll_offset, 0);
        let live = text(&s);
        assert_eq!(live[s.rows - 2], "line9");
        // scroll back two lines
        s.scroll_view(2);
        assert_eq!(s.scroll_offset, 2);
        let back: String = s.viewport_line(0).unwrap().iter().map(|c| c.ch).collect();
        assert!(back.trim_end().starts_with("line"));
    }

    #[test]
    fn alt_screen_switch() {
        let mut s = screen();
        s.feed(b"main");
        s.feed(b"\x1b[?1049h");
        assert!(s.alt_active);
        s.feed(b"alt");
        assert_eq!(text(&s)[0], "alt");
        s.feed(b"\x1b[?1049l");
        assert!(!s.alt_active);
        assert_eq!(text(&s)[0], "main");
    }

    #[test]
    fn utf8_multibyte() {
        let mut s = screen();
        s.feed("héllo".as_bytes());
        assert_eq!(text(&s)[0], "héllo");
        s.feed(" ✓".as_bytes());
        assert!(text(&s)[0].contains('✓'));
    }

    #[test]
    fn wrap_at_margin() {
        let mut s = screen();
        s.feed(b"012345678901234567890123");
        assert_eq!(text(&s)[0], "01234567890123456789");
        assert_eq!(text(&s)[1], "0123");
        assert_eq!(s.y, 1);
    }

    #[test]
    fn insert_and_delete_chars() {
        let mut s = screen();
        s.feed(b"abcdef\x1b[3G\x1b[2@");
        assert_eq!(text(&s)[0], "ab  cdef");
        s.feed(b"\x1b[2P");
        assert_eq!(text(&s)[0], "abcdef");
    }

    #[test]
    fn scroll_region() {
        let mut s = screen();
        s.feed(b"top\r\nmid1\r\nmid2\r\nbot\r\n");
        // Restrict scrolling to rows 2..4 (1-based: 2;4)
        s.feed(b"\x1b[2;4r");
        s.feed(b"\x1b[2;1H");
        s.feed(b"\x1b[M"); // delete line at row 2 (reverse index-ish: IL)
        let lines = text(&s);
        assert_eq!(lines[0], "top");
        // row 2 pulled up from row 3
        assert_eq!(lines[1], "mid2");
    }

    #[test]
    fn title_osc() {
        let mut s = screen();
        s.feed(b"\x1b]2;my title\x07");
        assert_eq!(s.title, "my title");
        assert!(s.title_changed);
        s.feed(b"\x1b]0;other\x1b\\");
        assert_eq!(s.title, "other");
    }

    #[test]
    fn device_status_report() {
        let mut s = screen();
        s.feed(b"\x1b[5n");
        let mut s2 = screen();
        let reply = s2.feed(b"\x1b[2;3H\x1b[6n");
        assert_eq!(reply, b"\x1b[2;3R");
        let _ = s;
    }

    #[test]
    fn private_modes() {
        let mut s = screen();
        s.feed(b"\x1b[?25l");
        assert!(!s.cursor_visible);
        s.feed(b"\x1b[?25h");
        assert!(s.cursor_visible);
        s.feed(b"\x1b[?1h");
        assert!(s.app_cursor_keys);
        s.feed(b"\x1b[?1l");
        assert!(!s.app_cursor_keys);
        s.feed(b"\x1b[?2004h");
        assert!(s.bracketed_paste);
    }

    #[test]
    fn resize_preserves_content() {
        let mut s = screen();
        s.feed(b"hello world");
        s.resize(10, 3);
        let line: String = s.viewport_line(0).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(line.trim_end(), "hello worl");
        assert_eq!(s.cols, 10);
        assert_eq!(s.rows, 3);
    }
}
