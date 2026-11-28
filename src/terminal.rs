use crate::profile::{EmulationType, RgbColor, TerminalProfile};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;

/// Custom heap-backed VT100/Xterm/OSC Escape Sequence Tokenizer
/// (Zero fixed-size stack arrays; supports arbitrarily large OSC 52 clipboard payloads)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    OscString,
    OscEscape,
    IgnoreUntilSt,
    IgnoreEscape,
}

#[derive(Debug, Clone)]
pub struct VtStreamParser {
    state: ParseState,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_expected: usize,
    intermediates: Vec<u8>,
    csi_buf: Vec<u8>,
    osc_buf: Vec<u8>,
}

impl Default for VtStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl VtStreamParser {
    pub fn new() -> Self {
        Self {
            state: ParseState::Ground,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            intermediates: Vec::with_capacity(4),
            csi_buf: Vec::with_capacity(64),
            osc_buf: Vec::with_capacity(256),
        }
    }
}

/// Color specification for a cell foreground or background
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    DefaultFg,
    DefaultBg,
    Indexed(u8),
    Rgb(RgbColor),
}

impl CellColor {
    pub fn resolve(&self, profile: &TerminalProfile, is_bold_fg: bool) -> RgbColor {
        match *self {
            CellColor::DefaultFg => profile.foreground,
            CellColor::DefaultBg => profile.background,
            CellColor::Indexed(idx) => {
                // Bold with standard colors 0..7 often maps to bright 8..15
                let actual_idx = if is_bold_fg && idx < 8 { idx + 8 } else { idx };
                profile.color_from_index(actual_idx)
            }
            CellColor::Rgb(rgb) => {
                if !profile.emulation.supports_ansi_color() {
                    profile.foreground
                } else {
                    rgb
                }
            }
        }
    }
}

/// Visual attributes of a single terminal character cell
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellAttrs {
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

impl Default for CellAttrs {
    fn default() -> Self {
        Self {
            fg: CellColor::DefaultFg,
            bg: CellColor::DefaultBg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            hidden: false,
            strikethrough: false,
        }
    }
}

/// A single terminal grid cell supporting full Unicode (grapheme clusters + wide characters)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Base character
    pub ch: char,
    /// Optional zero-width combining characters attached to this cell
    pub combining: String,
    /// Visual attributes
    pub attrs: CellAttrs,
    /// Cell display width: 1 for normal, 2 for wide (CJK/Emoji), 0 for trailing spacer of a wide character
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            combining: String::new(),
            attrs: CellAttrs::default(),
            width: 1,
        }
    }
}

impl Cell {
    pub fn reset(&mut self, attrs: CellAttrs) {
        self.ch = ' ';
        self.combining.clear();
        self.attrs = attrs;
        self.width = 1;
    }

    pub fn display_text(&self) -> String {
        if self.width == 0 {
            return String::new();
        }
        if self.combining.is_empty() {
            self.ch.to_string()
        } else {
            let mut s = String::with_capacity(4 + self.combining.len());
            s.push(self.ch);
            s.push_str(&self.combining);
            s
        }
    }
}

/// A single row in the terminal grid or scrollback buffer
#[derive(Debug, Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize, attrs: CellAttrs) -> Self {
        let mut cell = Cell::default();
        cell.attrs = attrs;
        Self {
            cells: vec![cell; cols],
            wrapped: false,
        }
    }

    pub fn clear(&mut self, attrs: CellAttrs) {
        for cell in &mut self.cells {
            cell.reset(attrs);
        }
        self.wrapped = false;
    }

    pub fn resize(&mut self, cols: usize, attrs: CellAttrs) {
        let mut default_cell = Cell::default();
        default_cell.attrs = attrs;
        self.cells.resize(cols, default_cell);
    }
}

/// OSC 52 Remote Clipboard Event emitted by the terminal state machine
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc52Event {
    /// Remote application requested writing decoded UTF-8 text to local clipboard
    /// `selection` contains the target selection code (e.g., "c" for clipboard, "p" for primary)
    WriteClipboard { selection: String, text: String },
    /// Remote application requested reading the local clipboard (`OSC 52 ; <sel> ; ? ST`)
    ReadClipboardQuery { selection: String },
}

/// Format an OSC 52 clipboard response payload to send back over the PTY when queried
pub fn format_osc52_response(selection: &str, text: &str) -> Vec<u8> {
    let sel = if selection.is_empty() { "c" } else { selection };
    let encoded = BASE64.encode(text.as_bytes());
    format!("\x1b]52;{};{}\x1b\\", sel, encoded).into_bytes()
}

/// Saved cursor state for DECSC (`ESC 7`) / DECRC (`ESC 8`)
#[derive(Debug, Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    attrs: CellAttrs,
    origin_mode: bool,
    charset_g0_line_drawing: bool,
    charset_g1_line_drawing: bool,
    active_charset_g1: bool,
}

impl Default for SavedCursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            attrs: CellAttrs::default(),
            origin_mode: false,
            charset_g0_line_drawing: false,
            charset_g1_line_drawing: false,
            active_charset_g1: false,
        }
    }
}

/// Selection coordinates in absolute line space (`0..scrollback.len() + rows`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub abs_line: usize,
    pub col: usize,
}

impl GridPoint {
    pub fn ordered(a: Self, b: Self) -> (Self, Self) {
        if (a.abs_line, a.col) <= (b.abs_line, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// Complete Terminal Emulator Engine
pub struct TerminalEmulator {
    pub cols: usize,
    pub rows: usize,
    pub grid: Vec<Row>,
    pub alt_grid: Vec<Row>,
    pub scrollback: VecDeque<Row>,
    pub max_scrollback: usize,
    /// Scroll offset from bottom (0 = viewing live bottom screen, >0 = viewing scrollback)
    pub display_offset: usize,

    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub pending_wrap: bool,

    pub current_attrs: CellAttrs,
    pub scroll_top: usize,
    pub scroll_bottom: usize,

    // Terminal modes
    pub alt_screen_active: bool,
    pub origin_mode: bool,
    pub autowrap_mode: bool,
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
    pub bracketed_paste_mode: bool,
    pub newline_mode: bool,
    mouse_tracking_mode: Option<u16>,
    sgr_mouse: bool,

    // DEC Special Graphics line-drawing character sets (G0 / G1)
    pub charset_g0_line_drawing: bool,
    pub charset_g1_line_drawing: bool,
    pub active_charset_g1: bool,

    saved_cursor_primary: SavedCursor,
    saved_cursor_alt: SavedCursor,
    tab_stops: Vec<bool>,

    // Profile and outbound queues
    pub profile: TerminalProfile,
    pub title: Option<String>,
    pub osc52_events: Vec<Osc52Event>,
    pub pty_write_queue: Vec<u8>,
    pub bell_triggered: bool,

    // Text selection in absolute line coordinates
    pub selection_start: Option<GridPoint>,
    pub selection_end: Option<GridPoint>,

    parser: VtStreamParser,
}

impl TerminalEmulator {
    pub fn new(cols: usize, rows: usize, profile: TerminalProfile) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(2);
        let attrs = CellAttrs::default();
        let grid = (0..rows).map(|_| Row::new(cols, attrs)).collect();
        let alt_grid = (0..rows).map(|_| Row::new(cols, attrs)).collect();
        let mut tab_stops = vec![false; cols];
        for c in (0..cols).step_by(8) {
            tab_stops[c] = true;
        }
        let max_scrollback = profile.scrollback_lines;

        Self {
            cols,
            rows,
            grid,
            alt_grid,
            scrollback: VecDeque::with_capacity(max_scrollback.min(1024)),
            max_scrollback,
            display_offset: 0,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            pending_wrap: false,
            current_attrs: attrs,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            alt_screen_active: false,
            origin_mode: false,
            autowrap_mode: true,
            application_cursor_keys: false,
            application_keypad: false,
            bracketed_paste_mode: false,
            newline_mode: false,
            mouse_tracking_mode: None,
            sgr_mouse: false,
            charset_g0_line_drawing: false,
            charset_g1_line_drawing: false,
            active_charset_g1: false,
            saved_cursor_primary: SavedCursor::default(),
            saved_cursor_alt: SavedCursor::default(),
            tab_stops,
            profile,
            title: None,
            osc52_events: Vec::new(),
            pty_write_queue: Vec::new(),
            bell_triggered: false,
            selection_start: None,
            selection_end: None,
            parser: VtStreamParser::new(),
        }
    }

    /// Feed raw bytes from the PTY into our self-contained VT100/Xterm/OSC state machine
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.advance_byte(byte);
        }
    }

    fn advance_byte(&mut self, byte: u8) {
        // Handle C0 control characters in non-OSC states
        match self.parser.state {
            ParseState::Ground | ParseState::Escape | ParseState::EscapeIntermediate | ParseState::CsiEntry => {
                if byte == 0x1b {
                    self.parser.utf8_len = 0;
                    self.parser.intermediates.clear();
                    self.parser.csi_buf.clear();
                    self.parser.state = ParseState::Escape;
                    return;
                }
                if byte == 0x18 || byte == 0x1a {
                    // CAN / SUB cancel current escape sequence
                    self.parser.state = ParseState::Ground;
                    return;
                }
                if byte < 0x20 && self.parser.state != ParseState::Ground {
                    self.execute(byte);
                    return;
                }
            }
            _ => {}
        }

        match self.parser.state {
            ParseState::Ground => {
                if byte < 0x20 {
                    self.parser.utf8_len = 0;
                    self.execute(byte);
                } else if byte == 0x7f {
                    // DEL ignored in Ground
                } else if byte < 0x80 {
                    self.parser.utf8_len = 0;
                    self.print(byte as char);
                } else {
                    // UTF-8 multi-byte decoding
                    if (byte & 0b1110_0000) == 0b1100_0000 {
                        self.parser.utf8_buf[0] = byte;
                        self.parser.utf8_len = 1;
                        self.parser.utf8_expected = 2;
                    } else if (byte & 0b1111_0000) == 0b1110_0000 {
                        self.parser.utf8_buf[0] = byte;
                        self.parser.utf8_len = 1;
                        self.parser.utf8_expected = 3;
                    } else if (byte & 0b1111_1000) == 0b1111_0000 {
                        self.parser.utf8_buf[0] = byte;
                        self.parser.utf8_len = 1;
                        self.parser.utf8_expected = 4;
                    } else if (byte & 0b1100_0000) == 0b1000_0000 && self.parser.utf8_len > 0 {
                        let idx = self.parser.utf8_len;
                        if idx < 4 {
                            self.parser.utf8_buf[idx] = byte;
                            self.parser.utf8_len += 1;
                        }
                        if self.parser.utf8_len == self.parser.utf8_expected {
                            let buf_copy = self.parser.utf8_buf;
                            let len = self.parser.utf8_len;
                            self.parser.utf8_len = 0;
                            if let Ok(s) = std::str::from_utf8(&buf_copy[..len]) {
                                if let Some(ch) = s.chars().next() {
                                    self.print(ch);
                                }
                            } else {
                                self.print('\u{FFFD}');
                            }
                        }
                    } else {
                        self.parser.utf8_len = 0;
                        self.print('\u{FFFD}');
                    }
                }
            }
            ParseState::Escape => {
                match byte {
                    b'[' => {
                        self.parser.csi_buf.clear();
                        self.parser.intermediates.clear();
                        self.parser.state = ParseState::CsiEntry;
                    }
                    b']' => {
                        self.parser.osc_buf.clear();
                        self.parser.state = ParseState::OscString;
                    }
                    b'P' | b'X' | b'^' | b'_' => {
                        // DCS, SOS, PM, APC -> ignore until ST
                        self.parser.state = ParseState::IgnoreUntilSt;
                    }
                    0x20..=0x2f => {
                        self.parser.intermediates.clear();
                        self.parser.intermediates.push(byte);
                        self.parser.state = ParseState::EscapeIntermediate;
                    }
                    0x30..=0x7e => {
                        self.parser.state = ParseState::Ground;
                        self.esc_dispatch(&[], false, byte);
                    }
                    _ => {
                        self.parser.state = ParseState::Ground;
                    }
                }
            }
            ParseState::EscapeIntermediate => {
                match byte {
                    0x20..=0x2f => {
                        self.parser.intermediates.push(byte);
                    }
                    0x30..=0x7e => {
                        let inter = self.parser.intermediates.clone();
                        self.parser.state = ParseState::Ground;
                        self.esc_dispatch(&inter, false, byte);
                    }
                    _ => {
                        self.parser.state = ParseState::Ground;
                    }
                }
            }
            ParseState::CsiEntry => {
                match byte {
                    0x30..=0x3f => {
                        if self.parser.csi_buf.is_empty() && matches!(byte, b'?' | b'>' | b'<' | b'=') {
                            self.parser.intermediates.push(byte);
                        } else {
                            self.parser.csi_buf.push(byte);
                        }
                    }
                    0x20..=0x2f => {
                        self.parser.intermediates.push(byte);
                    }
                    0x40..=0x7e => {
                        let csi_raw = std::mem::take(&mut self.parser.csi_buf);
                        let inter = std::mem::take(&mut self.parser.intermediates);
                        self.parser.state = ParseState::Ground;
                        let parsed = Self::parse_csi_params(&csi_raw);
                        self.csi_dispatch(&parsed, &inter, byte as char);
                    }
                    _ => {}
                }
            }
            ParseState::OscString => {
                match byte {
                    0x07 => {
                        // BEL terminates OSC
                        let osc_raw = std::mem::take(&mut self.parser.osc_buf);
                        self.parser.state = ParseState::Ground;
                        self.dispatch_osc_raw(&osc_raw);
                    }
                    0x1b => {
                        self.parser.state = ParseState::OscEscape;
                    }
                    _ => {
                        self.parser.osc_buf.push(byte);
                    }
                }
            }
            ParseState::OscEscape => {
                if byte == b'\\' {
                    // ST (ESC \) terminates OSC
                    let osc_raw = std::mem::take(&mut self.parser.osc_buf);
                    self.parser.state = ParseState::Ground;
                    self.dispatch_osc_raw(&osc_raw);
                } else {
                    self.parser.osc_buf.clear();
                    self.parser.state = ParseState::Ground;
                }
            }
            ParseState::IgnoreUntilSt => {
                if byte == 0x07 {
                    self.parser.state = ParseState::Ground;
                } else if byte == 0x1b {
                    self.parser.state = ParseState::IgnoreEscape;
                }
            }
            ParseState::IgnoreEscape => {
                if byte == b'\\' {
                    self.parser.state = ParseState::Ground;
                } else {
                    self.parser.state = ParseState::IgnoreUntilSt;
                }
            }
        }
    }

    fn parse_csi_params(raw: &[u8]) -> Vec<Vec<u16>> {
        if raw.is_empty() {
            return Vec::new();
        }
        let s = std::str::from_utf8(raw).unwrap_or("");
        s.split(';')
            .map(|group| {
                group
                    .split(':')
                    .map(|sub| sub.parse::<u16>().unwrap_or(0))
                    .collect::<Vec<u16>>()
            })
            .collect()
    }

    fn dispatch_osc_raw(&mut self, raw: &[u8]) {
        // Split into up to 3 fields separated by ';' so base64 payloads stay intact
        let mut parts: Vec<&[u8]> = Vec::with_capacity(3);
        let mut rest = raw;
        for _ in 0..2 {
            if let Some(pos) = rest.iter().position(|&b| b == b';') {
                parts.push(&rest[..pos]);
                rest = &rest[pos + 1..];
            } else {
                parts.push(rest);
                rest = &[];
                break;
            }
        }
        if !rest.is_empty() || raw.ends_with(b";") {
            parts.push(rest);
        }
        self.osc_dispatch(&parts, false);
    }

    pub fn set_profile(&mut self, profile: TerminalProfile) {
        self.max_scrollback = profile.scrollback_lines;
        while self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
        }
        self.profile = profile;
    }

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(2);
        let new_rows = new_rows.max(2);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        let blank_attrs = CellAttrs::default();
        for row in &mut self.grid {
            row.resize(new_cols, blank_attrs);
        }
        for row in &mut self.alt_grid {
            row.resize(new_cols, blank_attrs);
        }

        if new_rows > self.rows {
            let added = new_rows - self.rows;
            for _ in 0..added {
                self.grid.push(Row::new(new_cols, blank_attrs));
                self.alt_grid.push(Row::new(new_cols, blank_attrs));
            }
        } else if new_rows < self.rows {
            let removed = self.rows - new_rows;
            if !self.alt_screen_active && self.cursor_row >= new_rows {
                let to_scroll = (self.cursor_row - new_rows + 1).min(removed);
                for _ in 0..to_scroll {
                    let top_row = self.grid.remove(0);
                    self.push_scrollback(top_row);
                    self.cursor_row = self.cursor_row.saturating_sub(1);
                }
            }
            self.grid.truncate(new_rows);
            self.alt_grid.truncate(new_rows);
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_top = 0;
        self.scroll_bottom = new_rows - 1;
        self.cursor_row = self.cursor_row.min(new_rows - 1);
        self.cursor_col = self.cursor_col.min(new_cols - 1);
        self.pending_wrap = false;

        self.tab_stops.resize(new_cols, false);
        for c in (0..new_cols).step_by(8) {
            self.tab_stops[c] = true;
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            return;
        }
        if self.scrollback.len() >= self.max_scrollback {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
    }

    pub fn scroll_up_in_region(&mut self, count: usize) {
        let top = self.scroll_top.min(self.rows - 1);
        let bottom = self.scroll_bottom.min(self.rows - 1);
        if top > bottom {
            return;
        }
        let blank_attrs = CellAttrs {
            bg: self.current_attrs.bg,
            ..CellAttrs::default()
        };
        for _ in 0..count {
            let removed = self.grid.remove(top);
            if top == 0 && !self.alt_screen_active {
                self.push_scrollback(removed);
            }
            self.grid.insert(bottom, Row::new(self.cols, blank_attrs));
        }
    }

    pub fn scroll_down_in_region(&mut self, count: usize) {
        let top = self.scroll_top.min(self.rows - 1);
        let bottom = self.scroll_bottom.min(self.rows - 1);
        if top > bottom {
            return;
        }
        let blank_attrs = CellAttrs {
            bg: self.current_attrs.bg,
            ..CellAttrs::default()
        };
        for _ in 0..count {
            self.grid.remove(bottom);
            self.grid.insert(top, Row::new(self.cols, blank_attrs));
        }
    }

    /// Translate DEC Special Graphics character when line-drawing charset is active
    fn map_charset(&self, c: char) -> char {
        let use_line_drawing = if self.active_charset_g1 {
            self.charset_g1_line_drawing
        } else {
            self.charset_g0_line_drawing
        };
        if !use_line_drawing {
            return c;
        }
        match c {
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'q' => '─',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'a' => '▒',
            '`' => '◆',
            'f' => '°',
            'g' => '±',
            '~' => '·',
            _ => c,
        }
    }

    fn clear_wide_char_overlap(&mut self, row: usize, col: usize) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let blank = CellAttrs::default();
        let w = self.grid[row].cells[col].width;
        if w == 2 && col + 1 < self.cols {
            self.grid[row].cells[col + 1].reset(blank);
        } else if w == 0 && col > 0 {
            self.grid[row].cells[col - 1].reset(blank);
        }
    }

    fn linefeed(&mut self) {
        self.pending_wrap = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up_in_region(1);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down_in_region(1);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
    }

    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if save_cursor {
            self.saved_cursor_primary = self.capture_cursor();
        }
        if !self.alt_screen_active {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            let blank = CellAttrs::default();
            for row in &mut self.grid {
                row.clear(blank);
            }
            self.alt_screen_active = true;
            self.display_offset = 0;
        }
    }

    fn exit_alt_screen(&mut self, restore_cursor: bool) {
        if self.alt_screen_active {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.alt_screen_active = false;
        }
        if restore_cursor {
            let saved = self.saved_cursor_primary;
            self.restore_cursor(saved);
        }
    }

    fn capture_cursor(&self) -> SavedCursor {
        SavedCursor {
            row: self.cursor_row,
            col: self.cursor_col,
            attrs: self.current_attrs,
            origin_mode: self.origin_mode,
            charset_g0_line_drawing: self.charset_g0_line_drawing,
            charset_g1_line_drawing: self.charset_g1_line_drawing,
            active_charset_g1: self.active_charset_g1,
        }
    }

    fn restore_cursor(&mut self, saved: SavedCursor) {
        self.cursor_row = saved.row.min(self.rows - 1);
        self.cursor_col = saved.col.min(self.cols - 1);
        self.current_attrs = saved.attrs;
        self.origin_mode = saved.origin_mode;
        self.charset_g0_line_drawing = saved.charset_g0_line_drawing;
        self.charset_g1_line_drawing = saved.charset_g1_line_drawing;
        self.active_charset_g1 = saved.active_charset_g1;
        self.pending_wrap = false;
    }

    pub fn encode_mouse_wheel(
        &self,
        up: bool,
        row: usize,
        col: usize,
        modifiers: u8,
    ) -> Option<Vec<u8>> {
        if self.mouse_tracking_mode.is_none() || self.display_offset != 0 {
            return None;
        }

        let button = (if up { 64 } else { 65 }) | (modifiers & 0x1c);
        let x = col.min(self.cols - 1) + 1;
        let y = row.min(self.rows - 1) + 1;
        if self.sgr_mouse {
            Some(format!("\x1b[<{button};{x};{y}M").into_bytes())
        } else if x <= 223 && y <= 223 {
            Some(vec![0x1b, b'[', b'M', button + 32, x as u8 + 32, y as u8 + 32])
        } else {
            None
        }
    }

    fn full_reset(&mut self) {
        let blank = CellAttrs::default();
        self.current_attrs = blank;
        for row in &mut self.grid {
            row.clear(blank);
        }
        for row in &mut self.alt_grid {
            row.clear(blank);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.cursor_visible = true;
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.alt_screen_active = false;
        self.origin_mode = false;
        self.autowrap_mode = true;
        self.application_cursor_keys = false;
        self.application_keypad = false;
        self.bracketed_paste_mode = false;
        self.mouse_tracking_mode = None;
        self.sgr_mouse = false;
        self.charset_g0_line_drawing = false;
        self.charset_g1_line_drawing = false;
        self.active_charset_g1 = false;
    }

    /// Return the total number of lines (scrollback + active grid)
    pub fn total_lines(&self) -> usize {
        if self.alt_screen_active {
            self.rows
        } else {
            self.scrollback.len() + self.rows
        }
    }

    /// Fetch a row by its absolute line index (`0..total_lines()`)
    pub fn row_at_abs_line(&self, abs_line: usize) -> Option<&Row> {
        if self.alt_screen_active {
            self.grid.get(abs_line)
        } else if abs_line < self.scrollback.len() {
            self.scrollback.get(abs_line)
        } else {
            self.grid.get(abs_line - self.scrollback.len())
        }
    }

    /// Fetch the visible row at viewport index `view_row` (`0..rows`) taking `display_offset` into account
    pub fn visible_row(&self, view_row: usize) -> (&Row, usize) {
        if self.alt_screen_active || self.display_offset == 0 {
            let abs = if self.alt_screen_active {
                view_row
            } else {
                self.scrollback.len() + view_row
            };
            (&self.grid[view_row], abs)
        } else {
            let offset = self.display_offset.min(self.scrollback.len());
            let start_abs = self.scrollback.len() - offset;
            let abs = start_abs + view_row;
            if abs < self.scrollback.len() {
                (&self.scrollback[abs], abs)
            } else {
                (&self.grid[abs - self.scrollback.len()], abs)
            }
        }
    }

    /// Check if a cell at `(abs_line, col)` is currently selected
    pub fn is_cell_selected(&self, abs_line: usize, col: usize) -> bool {
        let (Some(a), Some(b)) = (self.selection_start, self.selection_end) else {
            return false;
        };
        if a == b {
            return false;
        }
        let (start, end) = GridPoint::ordered(a, b);
        if abs_line < start.abs_line || abs_line > end.abs_line {
            return false;
        }
        if start.abs_line == end.abs_line {
            col >= start.col && col <= end.col
        } else if abs_line == start.abs_line {
            col >= start.col
        } else if abs_line == end.abs_line {
            col <= end.col
        } else {
            true
        }
    }

    /// Extract selected text as a UTF-8 String
    pub fn selected_text(&self) -> Option<String> {
        let (Some(a), Some(b)) = (self.selection_start, self.selection_end) else {
            return None;
        };
        if a == b {
            return None;
        }
        let (start, end) = GridPoint::ordered(a, b);
        let mut out = String::new();

        for abs_line in start.abs_line..=end.abs_line {
            let Some(row) = self.row_at_abs_line(abs_line) else {
                continue;
            };
            let c_start = if abs_line == start.abs_line {
                start.col.min(row.cells.len())
            } else {
                0
            };
            let c_end = if abs_line == end.abs_line {
                (end.col + 1).min(row.cells.len())
            } else {
                row.cells.len()
            };

            let mut line_str = String::new();
            for col in c_start..c_end {
                let cell = &row.cells[col];
                if cell.width == 0 {
                    continue;
                }
                line_str.push(cell.ch);
                if !cell.combining.is_empty() {
                    line_str.push_str(&cell.combining);
                }
            }
            if abs_line < end.abs_line && !row.wrapped {
                out.push_str(line_str.trim_end_matches(' '));
                out.push('\n');
            } else {
                if abs_line == end.abs_line {
                    out.push_str(line_str.trim_end_matches(' '));
                } else {
                    out.push_str(&line_str);
                }
            }
        }

        if out.is_empty() { None } else { Some(out) }
    }

    fn handle_sgr(&mut self, params: &[Vec<u16>]) {
        let mut iter = params.iter();
        let mut any = false;

        while let Some(sub) = iter.next() {
            any = true;
            let code = sub.first().copied().unwrap_or(0);
            match code {
                0 => self.current_attrs = CellAttrs::default(),
                1 => self.current_attrs.bold = true,
                2 => self.current_attrs.dim = true,
                3 => self.current_attrs.italic = true,
                4 => self.current_attrs.underline = true,
                7 => self.current_attrs.inverse = true,
                8 => self.current_attrs.hidden = true,
                9 => self.current_attrs.strikethrough = true,
                21 | 22 => {
                    self.current_attrs.bold = false;
                    self.current_attrs.dim = false;
                }
                23 => self.current_attrs.italic = false,
                24 => self.current_attrs.underline = false,
                27 => self.current_attrs.inverse = false,
                28 => self.current_attrs.hidden = false,
                29 => self.current_attrs.strikethrough = false,
                30..=37 => {
                    self.current_attrs.fg = CellColor::Indexed((code - 30) as u8);
                }
                38 => {
                    // Foreground 256-color or 24-bit RGB
                    // Supports both semicolon-separated (38;5;n / 38;2;r;g;b) and colon-separated subparams (38:5:n / 38:2:r:g:b)
                    if sub.len() >= 3 {
                        if sub[1] == 5 {
                            self.current_attrs.fg = CellColor::Indexed(sub[2] as u8);
                        } else if sub[1] == 2 && sub.len() >= 5 {
                            let offset = if sub.len() >= 6 { 3 } else { 2 };
                            self.current_attrs.fg = CellColor::Rgb(RgbColor::new(
                                sub[offset] as u8,
                                sub[offset + 1] as u8,
                                sub[offset + 2] as u8,
                            ));
                        }
                    } else if let Some(kind_sub) = iter.next() {
                        let kind = kind_sub.first().copied().unwrap_or(0);
                        if kind == 5 {
                            if let Some(idx_sub) = iter.next() {
                                let idx = idx_sub.first().copied().unwrap_or(0) as u8;
                                self.current_attrs.fg = CellColor::Indexed(idx);
                            }
                        } else if kind == 2 {
                            let r = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            let g = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            let b = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            self.current_attrs.fg = CellColor::Rgb(RgbColor::new(r, g, b));
                        }
                    }
                }
                39 => self.current_attrs.fg = CellColor::DefaultFg,
                40..=47 => {
                    self.current_attrs.bg = CellColor::Indexed((code - 40) as u8);
                }
                48 => {
                    if sub.len() >= 3 {
                        if sub[1] == 5 {
                            self.current_attrs.bg = CellColor::Indexed(sub[2] as u8);
                        } else if sub[1] == 2 && sub.len() >= 5 {
                            let offset = if sub.len() >= 6 { 3 } else { 2 };
                            self.current_attrs.bg = CellColor::Rgb(RgbColor::new(
                                sub[offset] as u8,
                                sub[offset + 1] as u8,
                                sub[offset + 2] as u8,
                            ));
                        }
                    } else if let Some(kind_sub) = iter.next() {
                        let kind = kind_sub.first().copied().unwrap_or(0);
                        if kind == 5 {
                            if let Some(idx_sub) = iter.next() {
                                let idx = idx_sub.first().copied().unwrap_or(0) as u8;
                                self.current_attrs.bg = CellColor::Indexed(idx);
                            }
                        } else if kind == 2 {
                            let r = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            let g = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            let b = iter
                                .next()
                                .and_then(|s| s.first().copied())
                                .unwrap_or(0) as u8;
                            self.current_attrs.bg = CellColor::Rgb(RgbColor::new(r, g, b));
                        }
                    }
                }
                49 => self.current_attrs.bg = CellColor::DefaultBg,
                90..=97 => {
                    self.current_attrs.fg = CellColor::Indexed((code - 90 + 8) as u8);
                }
                100..=107 => {
                    self.current_attrs.bg = CellColor::Indexed((code - 100 + 8) as u8);
                }
                _ => {}
            }
        }

        if !any {
            self.current_attrs = CellAttrs::default();
        }
    }

    /// Encode a locally pressed key according to the active emulation profile & modes
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        if self.bracketed_paste_mode && !matches!(self.profile.emulation, EmulationType::Vt100) {
            let mut out = Vec::with_capacity(text.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            text.as_bytes().to_vec()
        }
    }

    fn print(&mut self, c: char) {
        let c = self.map_charset(c);
        let width = UnicodeWidthChar::width(c).unwrap_or(1);

        // Zero-width combining characters attach to the preceding cell
        if width == 0 {
            let target_col = if self.pending_wrap {
                self.cols.saturating_sub(1)
            } else {
                self.cursor_col.saturating_sub(1)
            };
            if self.cursor_row < self.rows && target_col < self.cols {
                let actual_col =
                    if self.grid[self.cursor_row].cells[target_col].width == 0 && target_col > 0 {
                        target_col - 1
                    } else {
                        target_col
                    };
                self.grid[self.cursor_row].cells[actual_col]
                    .combining
                    .push(c);
            }
            return;
        }

        if self.pending_wrap {
            if self.autowrap_mode {
                self.grid[self.cursor_row].wrapped = true;
                self.cursor_col = 0;
                self.linefeed();
            } else {
                self.cursor_col = self.cols - 1;
            }
            self.pending_wrap = false;
        }

        // If a wide character (width 2) starts at the last column, wrap to the next line first
        if width == 2 && self.cursor_col + 1 >= self.cols {
            if self.autowrap_mode {
                let blank = self.current_attrs;
                self.clear_wide_char_overlap(self.cursor_row, self.cursor_col);
                self.grid[self.cursor_row].cells[self.cursor_col].reset(blank);
                self.grid[self.cursor_row].wrapped = true;
                self.cursor_col = 0;
                self.linefeed();
            } else {
                return;
            }
        }

        let row = self.cursor_row;
        let col = self.cursor_col;
        self.clear_wide_char_overlap(row, col);
        if width == 2 && col + 1 < self.cols {
            self.clear_wide_char_overlap(row, col + 1);
        }

        self.grid[row].cells[col] = Cell {
            ch: c,
            combining: String::new(),
            attrs: self.current_attrs,
            width: width as u8,
        };

        if width == 2 && col + 1 < self.cols {
            self.grid[row].cells[col + 1] = Cell {
                ch: ' ',
                combining: String::new(),
                attrs: self.current_attrs,
                width: 0,
            };
        }

        if col + width >= self.cols {
            self.cursor_col = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.cursor_col += width;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {
                // BEL
                self.bell_triggered = true;
            }
            0x08 => {
                // BS (Backspace)
                self.pending_wrap = false;
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
            }
            0x09 => {
                // HT (Horizontal Tab)
                self.pending_wrap = false;
                let mut next = self.cursor_col + 1;
                while next < self.cols && !self.tab_stops[next] {
                    next += 1;
                }
                self.cursor_col = next.min(self.cols - 1);
            }
            0x0a | 0x0b | 0x0c => {
                // LF, VT, FF
                if self.newline_mode {
                    self.cursor_col = 0;
                }
                self.linefeed();
            }
            0x0d => {
                // CR
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            0x0e => {
                // SO (Shift Out -> G1 charset)
                self.active_charset_g1 = true;
            }
            0x0f => {
                // SI (Shift In -> G0 charset)
                self.active_charset_g1 = false;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        let cmd = std::str::from_utf8(params[0]).unwrap_or("");
        match cmd {
            "0" | "1" | "2" => {
                // Window / Tab Title
                if params.len() >= 2 {
                    let title = String::from_utf8_lossy(params[1]).into_owned();
                    self.title = Some(title);
                }
            }
            "52" => {
                // OSC 52 Remote Copy / Paste Protocol:
                // Format: ESC ] 52 ; <selection-targets> ; <base64-payload-or-?> ST
                // Or if extra semicolons appear, payload is in params[2]
                let selection = if params.len() >= 2 {
                    String::from_utf8_lossy(params[1]).into_owned()
                } else {
                    "c".to_string()
                };
                let payload_bytes = if params.len() >= 3 {
                    params[2]
                } else {
                    return;
                };
                let payload_str = std::str::from_utf8(payload_bytes).unwrap_or("").trim();

                if payload_str == "?" {
                    // Remote clipboard read query
                    if self.profile.allow_osc52_read {
                        self.osc52_events
                            .push(Osc52Event::ReadClipboardQuery { selection });
                    }
                } else if self.profile.allow_osc52_write {
                    // Strip any whitespace/newlines inside base64 payload (some tools split long base64 strings)
                    let cleaned: String =
                        payload_str.chars().filter(|c| !c.is_whitespace()).collect();
                    if cleaned.is_empty() {
                        self.osc52_events.push(Osc52Event::WriteClipboard {
                            selection,
                            text: String::new(),
                        });
                    } else if let Ok(decoded_bytes) = BASE64.decode(cleaned.as_bytes()) {
                        let text = String::from_utf8_lossy(&decoded_bytes).into_owned();
                        self.osc52_events
                            .push(Osc52Event::WriteClipboard { selection, text });
                    }
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &[Vec<u16>],
        intermediates: &[u8],
        action: char,
    ) {
        let first_param = |default: u16| -> usize {
            params
                .iter()
                .next()
                .and_then(|s| s.first().copied())
                .map(|v| if v == 0 { default } else { v })
                .unwrap_or(default) as usize
        };

        let is_private = intermediates.first() == Some(&b'?');
        let is_gt = intermediates.first() == Some(&b'>');

        if is_gt && action == 'c' {
            // Secondary Device Attributes (DA2)
            self.pty_write_queue
                .extend_from_slice(self.profile.emulation.da2_response());
            return;
        }

        if is_private {
            let set = match action {
                'h' => true,
                'l' => false,
                _ => return,
            };
            for sub in params.iter() {
                let mode = sub.first().copied().unwrap_or(0);
                match mode {
                    1 => self.application_cursor_keys = set,
                    6 => {
                        self.origin_mode = set;
                        self.cursor_row = if set { self.scroll_top } else { 0 };
                        self.cursor_col = 0;
                        self.pending_wrap = false;
                    }
                    7 => self.autowrap_mode = set,
                    12 | 25 => self.cursor_visible = set,
                    47 | 1047 => {
                        if set {
                            self.enter_alt_screen(false);
                        } else {
                            self.exit_alt_screen(false);
                        }
                    }
                    1000 | 1002 | 1003 => {
                        if set {
                            self.mouse_tracking_mode = Some(mode);
                        } else if self.mouse_tracking_mode == Some(mode) {
                            self.mouse_tracking_mode = None;
                        }
                    }
                    1006 => self.sgr_mouse = set,
                    1048 => {
                        if set {
                            if self.alt_screen_active {
                                self.saved_cursor_alt = self.capture_cursor();
                            } else {
                                self.saved_cursor_primary = self.capture_cursor();
                            }
                        } else {
                            let saved = if self.alt_screen_active {
                                self.saved_cursor_alt
                            } else {
                                self.saved_cursor_primary
                            };
                            self.restore_cursor(saved);
                        }
                    }
                    1049 => {
                        if set {
                            self.enter_alt_screen(true);
                        } else {
                            self.exit_alt_screen(true);
                        }
                    }
                    2004 => self.bracketed_paste_mode = set,
                    _ => {}
                }
            }
            return;
        }

        match action {
            'A' => {
                // CUU - Cursor Up
                let n = first_param(1);
                let min_row = if self.cursor_row >= self.scroll_top {
                    self.scroll_top
                } else {
                    0
                };
                self.cursor_row = self.cursor_row.saturating_sub(n).max(min_row);
                self.pending_wrap = false;
            }
            'B' | 'e' => {
                // CUD - Cursor Down
                let n = first_param(1);
                let max_row = if self.cursor_row <= self.scroll_bottom {
                    self.scroll_bottom
                } else {
                    self.rows - 1
                };
                self.cursor_row = (self.cursor_row + n).min(max_row);
                self.pending_wrap = false;
            }
            'C' | 'a' => {
                // CUF - Cursor Forward
                let n = first_param(1);
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
                self.pending_wrap = false;
            }
            'D' => {
                // CUB - Cursor Backward
                let n = first_param(1);
                self.cursor_col = self.cursor_col.saturating_sub(n);
                self.pending_wrap = false;
            }
            'E' => {
                // CNL - Cursor Next Line
                let n = first_param(1);
                self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            'F' => {
                // CPL - Cursor Preceding Line
                let n = first_param(1);
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            'G' | '`' => {
                // CHA - Cursor Character Absolute (1-based)
                let col = first_param(1).saturating_sub(1);
                self.cursor_col = col.min(self.cols - 1);
                self.pending_wrap = false;
            }
            'H' | 'f' => {
                // CUP / HVP - Cursor Position (1-based row;col)
                let mut it = params.iter();
                let r = it
                    .next()
                    .and_then(|s| s.first().copied())
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1) as usize;
                let c = it
                    .next()
                    .and_then(|s| s.first().copied())
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1) as usize;

                let base_row = if self.origin_mode {
                    self.scroll_top
                } else {
                    0
                };
                let max_row = if self.origin_mode {
                    self.scroll_bottom
                } else {
                    self.rows - 1
                };
                self.cursor_row = (base_row + r.saturating_sub(1)).min(max_row);
                self.cursor_col = c.saturating_sub(1).min(self.cols - 1);
                self.pending_wrap = false;
            }
            'J' => {
                // ED - Erase in Display
                let mode = params
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                let blank = CellAttrs {
                    bg: self.current_attrs.bg,
                    ..CellAttrs::default()
                };
                match mode {
                    0 => {
                        // Cursor to end of screen
                        for c in self.cursor_col..self.cols {
                            self.grid[self.cursor_row].cells[c].reset(blank);
                        }
                        for r in (self.cursor_row + 1)..self.rows {
                            self.grid[r].clear(blank);
                        }
                    }
                    1 => {
                        // Start of screen to cursor
                        for r in 0..self.cursor_row {
                            self.grid[r].clear(blank);
                        }
                        for c in 0..=self.cursor_col.min(self.cols - 1) {
                            self.grid[self.cursor_row].cells[c].reset(blank);
                        }
                    }
                    2 => {
                        // Entire screen
                        for r in 0..self.rows {
                            self.grid[r].clear(blank);
                        }
                    }
                    3 => {
                        // Entire screen + scrollback
                        for r in 0..self.rows {
                            self.grid[r].clear(blank);
                        }
                        self.scrollback.clear();
                        self.display_offset = 0;
                    }
                    _ => {}
                }
            }
            'K' => {
                // EL - Erase in Line
                let mode = params
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                let blank = CellAttrs {
                    bg: self.current_attrs.bg,
                    ..CellAttrs::default()
                };
                match mode {
                    0 => {
                        for c in self.cursor_col..self.cols {
                            self.grid[self.cursor_row].cells[c].reset(blank);
                        }
                    }
                    1 => {
                        for c in 0..=self.cursor_col.min(self.cols - 1) {
                            self.grid[self.cursor_row].cells[c].reset(blank);
                        }
                    }
                    2 => {
                        self.grid[self.cursor_row].clear(blank);
                    }
                    _ => {}
                }
            }
            'L' => {
                // IL - Insert Lines
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    let n = first_param(1);
                    let blank = CellAttrs {
                        bg: self.current_attrs.bg,
                        ..CellAttrs::default()
                    };
                    for _ in 0..n {
                        if self.scroll_bottom < self.grid.len() {
                            self.grid.remove(self.scroll_bottom);
                            self.grid
                                .insert(self.cursor_row, Row::new(self.cols, blank));
                        }
                    }
                }
            }
            'M' => {
                // DL - Delete Lines
                if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
                    let n = first_param(1);
                    let blank = CellAttrs {
                        bg: self.current_attrs.bg,
                        ..CellAttrs::default()
                    };
                    for _ in 0..n {
                        if self.cursor_row < self.grid.len() {
                            self.grid.remove(self.cursor_row);
                            self.grid
                                .insert(self.scroll_bottom, Row::new(self.cols, blank));
                        }
                    }
                }
            }
            '@' => {
                // ICH - Insert Blank Characters
                let n = first_param(1);
                let row = self.cursor_row;
                let col = self.cursor_col;
                let mut blank_cell = Cell::default();
                blank_cell.attrs = CellAttrs {
                    bg: self.current_attrs.bg,
                    ..CellAttrs::default()
                };
                for _ in 0..n {
                    if col < self.cols {
                        self.grid[row].cells.pop();
                        self.grid[row].cells.insert(col, blank_cell.clone());
                    }
                }
            }
            'P' => {
                // DCH - Delete Characters
                let n = first_param(1);
                let row = self.cursor_row;
                let col = self.cursor_col;
                let mut blank_cell = Cell::default();
                blank_cell.attrs = CellAttrs {
                    bg: self.current_attrs.bg,
                    ..CellAttrs::default()
                };
                for _ in 0..n {
                    if col < self.grid[row].cells.len() {
                        self.grid[row].cells.remove(col);
                        self.grid[row].cells.push(blank_cell.clone());
                    }
                }
            }
            'X' => {
                // ECH - Erase Characters
                let n = first_param(1);
                let blank = CellAttrs {
                    bg: self.current_attrs.bg,
                    ..CellAttrs::default()
                };
                let end = (self.cursor_col + n).min(self.cols);
                for c in self.cursor_col..end {
                    self.grid[self.cursor_row].cells[c].reset(blank);
                }
            }
            'S' => {
                // SU - Scroll Up
                let n = first_param(1);
                self.scroll_up_in_region(n);
            }
            'T' => {
                // SD - Scroll Down
                let n = first_param(1);
                self.scroll_down_in_region(n);
            }
            'd' => {
                // VPA - Line Position Absolute (1-based)
                let r = first_param(1).saturating_sub(1);
                self.cursor_row = r.min(self.rows - 1);
                self.pending_wrap = false;
            }
            'm' => {
                // SGR - Select Graphic Rendition
                self.handle_sgr(params);
            }
            'r' => {
                // DECSTBM - Set Top and Bottom Margins
                let mut it = params.iter();
                let top = it
                    .next()
                    .and_then(|s| s.first().copied())
                    .map(|v| if v == 0 { 1 } else { v })
                    .unwrap_or(1) as usize;
                let bottom = it
                    .next()
                    .and_then(|s| s.first().copied())
                    .map(|v| if v == 0 { self.rows as u16 } else { v })
                    .unwrap_or(self.rows as u16) as usize;

                let top_idx = top.saturating_sub(1).min(self.rows - 1);
                let bottom_idx = bottom.saturating_sub(1).min(self.rows - 1);
                if top_idx < bottom_idx {
                    self.scroll_top = top_idx;
                    self.scroll_bottom = bottom_idx;
                    self.cursor_row = if self.origin_mode {
                        self.scroll_top
                    } else {
                        0
                    };
                    self.cursor_col = 0;
                    self.pending_wrap = false;
                }
            }
            'c' => {
                // DA1 - Primary Device Attributes
                self.pty_write_queue
                    .extend_from_slice(self.profile.emulation.da1_response());
            }
            'n' => {
                // DSR - Device Status Report
                let code = first_param(0);
                match code {
                    5 => {
                        // Status OK
                        self.pty_write_queue.extend_from_slice(b"\x1b[0n");
                    }
                    6 => {
                        // CPR - Cursor Position Report (1-based)
                        let r = self.cursor_row + 1;
                        let c = self.cursor_col + 1;
                        let resp = format!("\x1b[{};{}R", r, c);
                        self.pty_write_queue.extend_from_slice(resp.as_bytes());
                    }
                    _ => {}
                }
            }
            'g' => {
                // TBC - Tabulation Clear
                let mode = params
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                if mode == 0 && self.cursor_col < self.cols {
                    self.tab_stops[self.cursor_col] = false;
                } else if mode == 3 {
                    self.tab_stops.fill(false);
                }
            }
            's' => {
                if self.alt_screen_active {
                    self.saved_cursor_alt = self.capture_cursor();
                } else {
                    self.saved_cursor_primary = self.capture_cursor();
                }
            }
            'u' => {
                let saved = if self.alt_screen_active {
                    self.saved_cursor_alt
                } else {
                    self.saved_cursor_primary
                };
                self.restore_cursor(saved);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates.first().copied(), byte) {
            (None, b'7') => {
                // DECSC - Save Cursor
                if self.alt_screen_active {
                    self.saved_cursor_alt = self.capture_cursor();
                } else {
                    self.saved_cursor_primary = self.capture_cursor();
                }
            }
            (None, b'8') => {
                // DECRC - Restore Cursor
                let saved = if self.alt_screen_active {
                    self.saved_cursor_alt
                } else {
                    self.saved_cursor_primary
                };
                self.restore_cursor(saved);
            }
            (None, b'D') => {
                // IND - Index
                self.linefeed();
            }
            (None, b'E') => {
                // NEL - Next Line
                self.cursor_col = 0;
                self.linefeed();
            }
            (None, b'H') => {
                // HTS - Horizontal Tab Set
                if self.cursor_col < self.cols {
                    self.tab_stops[self.cursor_col] = true;
                }
            }
            (None, b'M') => {
                // RI - Reverse Index
                self.reverse_index();
            }
            (None, b'c') => {
                // RIS - Full Reset
                self.full_reset();
            }
            (None, b'=') => {
                self.application_keypad = true;
            }
            (None, b'>') => {
                self.application_keypad = false;
            }
            (Some(b'('), b'0') => {
                // G0 = DEC Special Graphics
                self.charset_g0_line_drawing = true;
            }
            (Some(b'('), _) => {
                // G0 = ASCII
                self.charset_g0_line_drawing = false;
            }
            (Some(b')'), b'0') => {
                // G1 = DEC Special Graphics
                self.charset_g1_line_drawing = true;
            }
            (Some(b')'), _) => {
                // G1 = ASCII
                self.charset_g1_line_drawing = false;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{EmulationType, TerminalProfile};

    #[test]
    fn test_mouse_wheel_reporting_modes() {
        let profile = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
        let mut term = TerminalEmulator::new(240, 24, profile);
        term.process_bytes(b"\x1b[?1049h\x1b[?1006h");
        assert_eq!(term.encode_mouse_wheel(true, 2, 4, 0), None);

        for mode in [1000, 1002, 1003] {
            term.process_bytes(format!("\x1b[?{mode}h").as_bytes());
            assert_eq!(
                term.encode_mouse_wheel(true, 2, 4, 0).as_deref(),
                Some(b"\x1b[<64;5;3M".as_slice())
            );
            assert_eq!(
                term.encode_mouse_wheel(false, 99, 999, 28).as_deref(),
                Some(b"\x1b[<93;240;24M".as_slice())
            );
            term.process_bytes(format!("\x1b[?{mode}l").as_bytes());
            assert_eq!(term.encode_mouse_wheel(false, 2, 4, 0), None);
        }

        term.process_bytes(b"\x1b[?1049l\x1b[?1006l\x1b[?1000h");
        assert_eq!(
            term.encode_mouse_wheel(false, 2, 4, 0).as_deref(),
            Some(b"\x1b[Ma%#".as_slice())
        );
        assert_eq!(term.encode_mouse_wheel(true, 0, 223, 0), None);
        term.display_offset = 1;
        assert_eq!(term.encode_mouse_wheel(true, 0, 0, 0), None);
        term.display_offset = 0;
        term.process_bytes(b"\x1b[?1006h\x1bc\x1b[?1000h");
        assert_eq!(
            term.encode_mouse_wheel(true, 0, 0, 0).as_deref(),
            Some(b"\x1b[M`!!".as_slice())
        );
        term.process_bytes(b"\x1bc");
        assert_eq!(term.encode_mouse_wheel(true, 0, 0, 0), None);
    }

    #[test]
    fn test_osc52_write_and_query() {
        let profile = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
        let mut term = TerminalEmulator::new(80, 24, profile);

        // Remote app sends OSC 52 write ("Hello Remote World!" base64 = SGVsbG8gUmVtb3RlIFdvcmxkIQ==)
        term.process_bytes(b"\x1b]52;c;SGVsbG8gUmVtb3RlIFdvcmxkIQ==\x07");
        assert_eq!(term.osc52_events.len(), 1);
        assert_eq!(
            term.osc52_events[0],
            Osc52Event::WriteClipboard {
                selection: "c".to_string(),
                text: "Hello Remote World!".to_string(),
            }
        );

        // Remote app sends OSC 52 read query with ST terminator (ESC \)
        term.process_bytes(b"\x1b]52;p;?\x1b\\");
        assert_eq!(term.osc52_events.len(), 2);
        assert_eq!(
            term.osc52_events[1],
            Osc52Event::ReadClipboardQuery {
                selection: "p".to_string(),
            }
        );

        // Verify formatted response matches OSC 52 specification
        let resp = format_osc52_response("p", "ClipboardData");
        assert_eq!(resp, b"\x1b]52;p;Q2xpcGJvYXJkRGF0YQ==\x1b\\");
    }

    #[test]
    fn test_unicode_wide_and_combining_chars() {
        let profile = TerminalProfile::default();
        let mut term = TerminalEmulator::new(20, 5, profile);

        // Print CJK wide characters, emoji, and combining accent (e + \u{0301})
        term.process_bytes("日本語🚀e\u{0301}!".as_bytes());

        // '日' at col 0 (width 2), spacer at col 1 (width 0)
        assert_eq!(term.grid[0].cells[0].ch, '日');
        assert_eq!(term.grid[0].cells[0].width, 2);
        assert_eq!(term.grid[0].cells[1].width, 0);

        // '本' at col 2 (width 2), spacer at col 3
        assert_eq!(term.grid[0].cells[2].ch, '本');
        assert_eq!(term.grid[0].cells[2].width, 2);

        // '語' at col 4 (width 2), spacer at col 5
        assert_eq!(term.grid[0].cells[4].ch, '語');
        assert_eq!(term.grid[0].cells[4].width, 2);

        // '🚀' at col 6 (width 2), spacer at col 7
        assert_eq!(term.grid[0].cells[6].ch, '🚀');
        assert_eq!(term.grid[0].cells[6].width, 2);

        // 'e' with combining acute accent at col 8 (width 1)
        assert_eq!(term.grid[0].cells[8].ch, 'e');
        assert_eq!(term.grid[0].cells[8].combining, "\u{0301}");
        assert_eq!(term.grid[0].cells[8].display_text(), "e\u{0301}");

        // '!' at col 9
        assert_eq!(term.grid[0].cells[9].ch, '!');
    }

    #[test]
    fn test_profile_device_attributes_and_colors() {
        let vt100_profile = TerminalProfile::midnight_dark(EmulationType::Vt100);
        let mut vt100 = TerminalEmulator::new(80, 24, vt100_profile);
        vt100.process_bytes(b"\x1b[c");
        assert_eq!(vt100.pty_write_queue, b"\x1b[?1;2c");

        let xterm_profile = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
        let mut xterm = TerminalEmulator::new(80, 24, xterm_profile);
        xterm.process_bytes(b"\x1b[c");
        assert_eq!(xterm.pty_write_queue, b"\x1b[?62;1;2;6;7;8;9;15;22c");
    }

    #[test]
    fn test_sgr_colors_and_dec_line_drawing() {
        let profile = TerminalProfile::midnight_dark(EmulationType::Xterm256Color);
        let mut term = TerminalEmulator::new(40, 10, profile);

        // 24-bit RGB foreground (38;2;10;20;30) and 256-color background (48;5;208)
        term.process_bytes(b"\x1b[1;38;2;10;20;30;48;5;208mA\x1b[0m");
        let cell_a = &term.grid[0].cells[0];
        assert_eq!(cell_a.ch, 'A');
        assert!(cell_a.attrs.bold);
        assert_eq!(cell_a.attrs.fg, CellColor::Rgb(RgbColor::new(10, 20, 30)));
        assert_eq!(cell_a.attrs.bg, CellColor::Indexed(208));

        // Switch G0 to DEC Special Graphics line drawing (ESC ( 0) and draw 'l' 'q' 'k' -> '┌' '─' '┐'
        term.process_bytes(b"\x1b(0lqk\x1b(B");
        assert_eq!(term.grid[0].cells[1].ch, '┌');
        assert_eq!(term.grid[0].cells[2].ch, '─');
        assert_eq!(term.grid[0].cells[3].ch, '┐');
    }

    #[test]
    fn test_alt_screen_and_scrollback_selection() {
        let profile = TerminalProfile::midnight_dark(EmulationType::Xterm);
        let mut term = TerminalEmulator::new(10, 3, profile);

        term.process_bytes(b"Line1\r\nLine2\r\nLine3\r\nLine4");
        // Line1 should have scrolled into scrollback
        assert_eq!(term.scrollback.len(), 1);
        assert_eq!(term.scrollback[0].cells[0].ch, 'L');
        assert_eq!(term.scrollback[0].cells[4].ch, '1');

        // Select across scrollback line 0 ("Line1") and active line 1 ("Line2")
        term.selection_start = Some(GridPoint { abs_line: 0, col: 0 });
        term.selection_end = Some(GridPoint { abs_line: 1, col: 4 });
        assert_eq!(term.selected_text().as_deref(), Some("Line1\nLine2"));

        // Enter alternate screen (?1049h) and move cursor home, verify clean grid, then exit (?1049l) and verify restoration
        term.process_bytes(b"\x1b[?1049h\x1b[HAltScreen");
        assert!(term.alt_screen_active);
        assert_eq!(term.grid[0].cells[0].ch, 'A');

        term.process_bytes(b"\x1b[?1049l");
        assert!(!term.alt_screen_active);
        assert_eq!(term.grid[0].cells[4].ch, '2');
    }
}
