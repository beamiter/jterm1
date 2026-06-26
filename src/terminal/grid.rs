//! Minimal offline terminal grid emulator.
//!
//! Captured command output bytes carry the cursor-positioning escape sequences
//! a program emitted, but `strip_ansi` deletes those escapes without applying
//! them. For commands that REPAINT the screen — `less -X` (the no-alt-screen
//! path many distros configure for `git log`/`man`), top, watch — the raw byte
//! stream collapses into stacked, duplicated text once stripped (visible as
//! "the same commit appears 4 times" in a recorded `git log` block).
//!
//! This module replays the bytes onto a fixed-size character grid, applying
//! the escapes the command actually used (cursor moves, clear-screen,
//! clear-line, scroll). The final grid is what the user actually saw on
//! screen; that is what the recorded block should show.
//!
//! Scope: enough to handle pagers and dashboards. Color/SGR is dropped (we
//! only return text). Unsupported sequences are skipped without aborting.

use std::collections::HashMap;

const MIN_COLS: usize = 1;
const MIN_ROWS: usize = 1;

/// True if `bytes` contain any CSI escape that moves the cursor or clears
/// the screen — i.e. anything `strip_ansi` would silently lose. Used to
/// short-circuit the emulator: plain streamed output (no repaints) is fine
/// to display via the existing `strip_ansi` path.
pub fn has_cursor_positioning(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                // Scan to the final byte (0x40..=0x7e) and inspect it.
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    return false;
                }
                let final_b = bytes[j];
                // Cursor moves, clears, scroll regions — anything that makes the
                // text-order stream lie about the visible result.
                if matches!(
                    final_b,
                    b'H' | b'f'
                        | b'A'
                        | b'B'
                        | b'C'
                        | b'D'
                        | b'E'
                        | b'F'
                        | b'G'
                        | b'd'
                        | b'`'
                        | b'J'
                        | b'K'
                        | b'r'
                        | b's'
                        | b'u'
                        | b'L'
                        | b'M'
                        | b'S'
                        | b'T'
                        | b'@'
                        | b'P'
                        | b'X'
                        | b'b'
                ) {
                    return true;
                }
                i = j + 1;
            }
            b'M' => return true, // RI (reverse index) scrolls
            b'P' | b'_' => {
                // DCS (sixel) or APC (kitty graphics) — `strip_ansi` doesn't
                // know how to consume the binary payload, so route to the grid
                // emulator which terminator-aware-skips it and stamps a
                // placeholder.
                return true;
            }
            b']' => {
                // Skip OSC string to terminator (BEL or ESC \).
                let mut j = i + 2;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            _ => i += 2,
        }
    }
    false
}

/// Replay `bytes` onto a `cols × rows` grid and return the resulting text.
/// Trailing whitespace per row is trimmed; trailing blank rows are dropped.
/// Equivalent to `render_to_ansi` with an empty palette — colors are dropped.
pub fn render_to_text(bytes: &[u8], cols: usize, rows: usize) -> String {
    render_to_ansi(bytes, cols, rows, &default_palette())
}

fn default_palette() -> [gtk4::gdk::RGBA; 16] {
    [gtk4::gdk::RGBA::BLACK; 16]
}

/// Replay `bytes` onto a `cols × rows` grid and return the resulting text WITH
/// re-emitted SGR escapes, so colorized pager output keeps its colors when the
/// recorded block is rendered. The `palette` is needed to map indexed colors
/// (SGR 30-37/40-47/90-97/100-107 + 38;5/48;5) to RGB.
pub fn render_to_ansi(
    bytes: &[u8],
    cols: usize,
    rows: usize,
    palette: &[gtk4::gdk::RGBA; 16],
) -> String {
    let cols = cols.max(MIN_COLS);
    let rows = rows.max(MIN_ROWS);
    let mut grid = Grid::new(cols, rows, *palette);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                if i + 1 >= bytes.len() {
                    break;
                }
                match bytes[i + 1] {
                    b'[' => {
                        let mut j = i + 2;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            break;
                        }
                        let params = &bytes[i + 2..j];
                        let final_b = bytes[j];
                        apply_csi(&mut grid, params, final_b);
                        i = j + 1;
                    }
                    b']' => {
                        // Find the OSC payload bounds and its terminator.
                        let body_start = i + 2;
                        let mut j = body_start;
                        let mut term_end = j;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                term_end = j;
                                j += 1;
                                break;
                            }
                            if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                                term_end = j;
                                j += 2;
                                break;
                            }
                            j += 1;
                        }
                        // OSC 8 hyperlinks: payload is `8;<params>;<uri>`. We
                        // capture <uri> into cur_state so subsequent cells get
                        // tagged with the link, and `into_text` re-emits OSC 8
                        // around runs of linked cells. All other OSCs (0/1/2
                        // titles, 4/10/11 color queries, 52 clipboard, 133
                        // shell integration) are not visible state for the
                        // rendered grid and stay dropped.
                        let payload = &bytes[body_start..term_end];
                        if payload.starts_with(b"8;") {
                            let rest = &payload[2..];
                            if let Some(pos) = rest.iter().position(|&b| b == b';') {
                                let uri = &rest[pos + 1..];
                                grid.cur_state.hyperlink = if uri.is_empty() {
                                    None
                                } else {
                                    Some(String::from_utf8_lossy(uri).into_owned())
                                };
                                grid.cur_style = grid.intern_style();
                            }
                        }
                        i = j;
                    }
                    b'M' => {
                        grid.reverse_index();
                        i += 2;
                    }
                    b'D' => {
                        grid.line_feed();
                        i += 2;
                    }
                    b'E' => {
                        grid.line_feed();
                        grid.col = 0;
                        i += 2;
                    }
                    b'7' => {
                        // DECSC — save cursor + SGR + charset + scroll region.
                        grid.save_cursor();
                        i += 2;
                    }
                    b'8' => {
                        // DECRC — restore the snapshot from the last DECSC.
                        grid.restore_cursor();
                        i += 2;
                    }
                    b'P' | b'_' | b'^' | b'X' => {
                        // DCS (\eP), APC (\e_), PM (\e^), SOS (\eX) — string
                        // sequences terminated by ST (\e\) or BEL. The most
                        // common producers are sixel images (\eP...q...\e\)
                        // and the kitty graphics protocol (\e_G...;...\e\).
                        // Skip the payload entirely; without this branch the
                        // default `_ => i += 2` arm would dump the binary
                        // payload as text into the rendered block. When the
                        // payload is non-trivial, emit a short placeholder so
                        // the user knows a graphic was here.
                        let kind = bytes[i + 1];
                        let body_start = i + 2;
                        let mut j = body_start;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                break;
                            }
                            if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                                break;
                            }
                            j += 1;
                        }
                        let payload_len = j.saturating_sub(body_start);
                        // Skip past terminator if present.
                        let skip_term = if j < bytes.len() {
                            if bytes[j] == 0x07 {
                                1
                            } else {
                                2
                            }
                        } else {
                            0
                        };
                        // Emit a placeholder only for sizable graphics-style
                        // payloads (DCS and APC). PM/SOS are rare and usually
                        // empty; skip silently.
                        if payload_len > 16 && (kind == b'P' || kind == b'_') {
                            let label = if kind == b'_' { "[graphic]" } else { "[image]" };
                            for c in label.chars() {
                                grid.put_char(c);
                            }
                        }
                        i = j + skip_term;
                    }
                    b'(' | b')' | b'*' | b'+' => {
                        // SCS — Select Character Set. ESC ( c designates G0,
                        // ESC ) c designates G1. We support the bare minimum:
                        // `0` = DEC special line-drawing graphics, anything
                        // else = ASCII (the default). Pagers, dialog boxes,
                        // `tput`, and TUI frames all rely on this to draw
                        // borders; without it lines render as raw letters.
                        if i + 2 < bytes.len() {
                            let target = bytes[i + 1];
                            let cs = bytes[i + 2];
                            let line_drawing = cs == b'0';
                            match target {
                                b'(' => grid.g0_line_drawing = line_drawing,
                                b')' => grid.g1_line_drawing = line_drawing,
                                _ => {}
                            }
                        }
                        i += 3;
                    }
                    _ => i += 2,
                }
            }
            0x0e => {
                // SO: invoke G1 into GL.
                grid.gl_is_g1 = true;
                i += 1;
            }
            0x0f => {
                // SI: invoke G0 into GL.
                grid.gl_is_g1 = false;
                i += 1;
            }
            b'\n' => {
                // Treat LF as CR+LF for output: PTYs run with ONLCR by default,
                // and matching that here keeps streamed output column-aligned
                // (without it, "line1\nline2" puts "line2" at the column after
                // "line1" instead of at column 0).
                grid.line_feed();
                grid.col = 0;
                i += 1;
            }
            b'\r' => {
                grid.col = 0;
                i += 1;
            }
            b'\x08' => {
                if grid.col > 0 {
                    grid.col -= 1;
                }
                i += 1;
            }
            b'\x07' | b'\x00' => i += 1,
            b'\t' => {
                let next = ((grid.col / 8) + 1) * 8;
                grid.col = next.min(cols - 1);
                i += 1;
            }
            _ => {
                // UTF-8: read a full codepoint to keep multi-byte chars together.
                let len = utf8_char_len(b);
                let end = (i + len).min(bytes.len());
                if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
                    if let Some(c) = s.chars().next() {
                        grid.put_char(c);
                    }
                }
                i = end;
            }
        }
    }
    grid.into_text()
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xc0 {
        1
    } else if b < 0xe0 {
        2
    } else if b < 0xf0 {
        3
    } else {
        4
    }
}

fn parse_params(params: &[u8]) -> Vec<u32> {
    if params.is_empty() {
        return Vec::new();
    }
    // Drop a leading private marker (`?`, `>`, `=`) so `\e[?25h` style sequences parse.
    let mut p = params;
    if matches!(p.first(), Some(&b'?') | Some(&b'>') | Some(&b'=')) {
        p = &p[1..];
    }
    p.split(|&b| b == b';' || b == b':')
        .map(|chunk| {
            std::str::from_utf8(chunk)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0)
        })
        .collect()
}

fn apply_csi(grid: &mut Grid, params: &[u8], final_b: u8) {
    // SGR (m) updates the current cell style. Routed here before parsing as
    // u32 so colon-delimited sub-parameters survive into `parse_sgr_params`.
    if final_b == b'm' {
        if params.is_empty() {
            super::ansi::parse_sgr_params(&mut grid.cur_state, b"0", &grid.palette);
        } else {
            super::ansi::parse_sgr_params(&mut grid.cur_state, params, &grid.palette);
        }
        grid.cur_style = grid.intern_style();
        return;
    }
    let p = parse_params(params);
    let p1 = |i: usize, dflt: u32| p.get(i).copied().filter(|v| *v != 0).unwrap_or(dflt);
    let p1_0 = |i: usize| p.get(i).copied().unwrap_or(0);
    match final_b {
        b'H' | b'f' => {
            let row = p1(0, 1).saturating_sub(1) as usize;
            let col = p1(1, 1).saturating_sub(1) as usize;
            grid.row = row.min(grid.rows - 1);
            grid.col = col.min(grid.cols - 1);
        }
        b'A' => {
            let n = p1(0, 1) as usize;
            grid.row = grid.row.saturating_sub(n);
        }
        b'B' | b'e' => {
            let n = p1(0, 1) as usize;
            grid.row = (grid.row + n).min(grid.rows - 1);
        }
        b'C' | b'a' => {
            let n = p1(0, 1) as usize;
            grid.col = (grid.col + n).min(grid.cols - 1);
        }
        b'D' => {
            let n = p1(0, 1) as usize;
            grid.col = grid.col.saturating_sub(n);
        }
        b'E' => {
            let n = p1(0, 1) as usize;
            grid.row = (grid.row + n).min(grid.rows - 1);
            grid.col = 0;
        }
        b'F' => {
            let n = p1(0, 1) as usize;
            grid.row = grid.row.saturating_sub(n);
            grid.col = 0;
        }
        b'G' | b'`' => {
            let col = p1(0, 1).saturating_sub(1) as usize;
            grid.col = col.min(grid.cols - 1);
        }
        b'd' => {
            let row = p1(0, 1).saturating_sub(1) as usize;
            grid.row = row.min(grid.rows - 1);
        }
        b'J' => match p1_0(0) {
            0 => grid.erase_below(),
            1 => grid.erase_above(),
            2 | 3 => grid.erase_all(),
            _ => {}
        },
        b'K' => match p1_0(0) {
            0 => grid.erase_line_right(),
            1 => grid.erase_line_left(),
            2 => grid.erase_line(),
            _ => {}
        },
        b'L' => grid.insert_lines(p1(0, 1) as usize),
        b'M' => grid.delete_lines(p1(0, 1) as usize),
        b'S' => grid.scroll_up(p1(0, 1) as usize),
        b'T' => grid.scroll_down(p1(0, 1) as usize),
        // ICH (CSI Pn @) — insert N blank chars at cursor, shift rest right.
        b'@' => grid.insert_chars(p1(0, 1) as usize),
        // DCH (CSI Pn P) — delete N chars at cursor, shift rest left.
        b'P' => grid.delete_chars(p1(0, 1) as usize),
        // ECH (CSI Pn X) — erase N chars starting at cursor in place.
        b'X' => grid.erase_chars(p1(0, 1) as usize),
        // REP (CSI Pn b) — repeat the last printable char N times.
        b'b' => grid.repeat_last(p1(0, 1) as usize),
        b's' => grid.save_cursor(),
        b'u' => grid.restore_cursor(),
        b'r' => {
            let top = p1(0, 1).saturating_sub(1) as usize;
            let bot = p
                .get(1)
                .copied()
                .filter(|v| *v != 0)
                .map(|v| v as usize - 1)
                .unwrap_or(grid.rows - 1);
            grid.scroll_top = top.min(grid.rows - 1);
            grid.scroll_bot = bot.min(grid.rows - 1);
        }
        // m (SGR), h/l (modes), n (DSR) — ignored.
        _ => {}
    }
}

/// One grid cell: a character + an index into `Grid::style_table`. Default
/// style (index 0) is always present and represents "no SGR" — plain output.
type Cell = (char, u16);

const DEFAULT_STYLE: u16 = 0;

fn blank_cell() -> Cell {
    (' ', DEFAULT_STYLE)
}

/// Hashable digest of `AnsiStyleState`. `RGBA` holds f32 fields and isn't
/// `Hash`/`Eq`, so we project colors down to packed `u32`s (R<<24|G<<16|B<<8|A
/// each rounded from the 0..1 float) and pack the booleans + underline style
/// into a single `u16` flag word. Two styles compare equal under this key iff
/// they would produce the same SGR sequence, which is the equivalence we want
/// for interning.
#[derive(Clone, PartialEq, Eq, Hash)]
struct StyleKey {
    fg: Option<u32>,
    bg: Option<u32>,
    uc: Option<u32>,
    flags: u16,
    link: Option<String>,
}

#[inline]
fn pack_rgba(c: &gtk4::gdk::RGBA) -> u32 {
    ((c.red() * 255.0).round() as u32) << 24
        | ((c.green() * 255.0).round() as u32) << 16
        | ((c.blue() * 255.0).round() as u32) << 8
        | ((c.alpha() * 255.0).round() as u32)
}

fn style_key(s: &super::ansi::AnsiStyleState) -> StyleKey {
    let mut flags: u16 = 0;
    if s.bold {
        flags |= 1 << 0;
    }
    if s.italic {
        flags |= 1 << 1;
    }
    if s.strikethrough {
        flags |= 1 << 2;
    }
    if s.dim {
        flags |= 1 << 3;
    }
    if s.reverse {
        flags |= 1 << 4;
    }
    if s.hidden {
        flags |= 1 << 5;
    }
    if s.overline {
        flags |= 1 << 6;
    }
    if s.blink {
        flags |= 1 << 7;
    }
    flags |= (s.underline_style as u16) << 8;
    StyleKey {
        fg: s.foreground.as_ref().map(pack_rgba),
        bg: s.background.as_ref().map(pack_rgba),
        uc: s.underline_color.as_ref().map(pack_rgba),
        flags,
        link: s.hyperlink.clone(),
    }
}

/// Cursor + style + scroll-region snapshot taken by DECSC (ESC 7) / CSI s
/// and restored by DECRC (ESC 8) / CSI u. xterm/VTE save SGR and charset
/// state alongside the cursor; jterm1 matches that so e.g. `dialog` and
/// `fzf` restore their colors correctly after popping back from a submenu.
#[derive(Clone)]
struct SavedState {
    row: usize,
    col: usize,
    scroll_top: usize,
    scroll_bot: usize,
    cur_style: u16,
    cur_state: super::ansi::AnsiStyleState,
    g0_line_drawing: bool,
    g1_line_drawing: bool,
    gl_is_g1: bool,
}

struct Grid {
    cells: Vec<Vec<Cell>>,
    cols: usize,
    rows: usize,
    row: usize,
    col: usize,
    saved: Option<SavedState>,
    scroll_top: usize,
    scroll_bot: usize,
    /// Highest row index that has ever held non-blank content. The output
    /// trims to this so a 24-row grid receiving 5 lines doesn't pad with 19
    /// blanks.
    high_water: usize,
    /// Interned styles — cells store a u16 index, the table holds unique states.
    /// Index 0 is the default (no SGR), so plain output skips the table entirely.
    style_table: Vec<super::ansi::AnsiStyleState>,
    /// O(1) reverse lookup so `intern_style` doesn't linearly scan
    /// `style_table` per SGR — colored output (top, htop, fzf preview) can
    /// blow the table up to thousands of entries.
    style_index: HashMap<StyleKey, u16>,
    /// Currently-active SGR state (mutated by `m`-final CSI sequences).
    cur_state: super::ansi::AnsiStyleState,
    /// Interned index of `cur_state` — what new cells are written with.
    cur_style: u16,
    /// Palette used to resolve indexed SGR colors (30-37/40-47/90-97/100-107
    /// and 38;5/48;5) to RGB. Threaded in from the active terminal's theme.
    palette: [gtk4::gdk::RGBA; 16],
    /// Last printable char written — REP (CSI Pn b) repeats this.
    last_char: Option<char>,
    /// DEC special line-drawing charset selected for G0/G1 (`ESC ( 0` etc.).
    /// When the active GL slot is line-drawing, ASCII letters in j..x map to
    /// box-drawing glyphs.
    g0_line_drawing: bool,
    g1_line_drawing: bool,
    /// Which slot GL currently points at — SI (0x0f) invokes G0, SO (0x0e) G1.
    gl_is_g1: bool,
}

impl Grid {
    fn new(cols: usize, rows: usize, palette: [gtk4::gdk::RGBA; 16]) -> Self {
        Self {
            cells: vec![vec![blank_cell(); cols]; rows],
            cols,
            rows,
            row: 0,
            col: 0,
            saved: None,
            scroll_top: 0,
            scroll_bot: rows - 1,
            high_water: 0,
            style_table: vec![super::ansi::AnsiStyleState::default()],
            style_index: HashMap::new(),
            cur_state: super::ansi::AnsiStyleState::default(),
            cur_style: DEFAULT_STYLE,
            palette,
            last_char: None,
            g0_line_drawing: false,
            g1_line_drawing: false,
            gl_is_g1: false,
        }
    }

    /// Cell used by erase/insert operations. Honors BCE: if the current SGR
    /// state has a non-default background, the "blank" cells inherit it so
    /// status-bar painting that clears with bg-color survives the replay.
    fn bg_cell(&mut self) -> Cell {
        if self.cur_state.background.is_some() {
            (' ', self.cur_style)
        } else {
            blank_cell()
        }
    }

    fn line_drawing_active(&self) -> bool {
        if self.gl_is_g1 {
            self.g1_line_drawing
        } else {
            self.g0_line_drawing
        }
    }

    /// Map a DEC special-graphics character to its Unicode box-drawing glyph.
    fn translate_line_drawing(c: char) -> char {
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
            'o' => '⎺',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            _ => c,
        }
    }

    /// Find or insert `cur_state` in the style table and return its index.
    fn intern_style(&mut self) -> u16 {
        if self.cur_state == self.style_table[DEFAULT_STYLE as usize] {
            return DEFAULT_STYLE;
        }
        let key = style_key(&self.cur_state);
        if let Some(&i) = self.style_index.get(&key) {
            return i;
        }
        if self.style_table.len() >= u16::MAX as usize {
            return (self.style_table.len() - 1) as u16;
        }
        let i = self.style_table.len() as u16;
        self.style_table.push(self.cur_state.clone());
        self.style_index.insert(key, i);
        i
    }

    fn put_char(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        // Line-drawing charset translates a small set of ASCII letters into
        // box-drawing glyphs. Apply before width measurement.
        let c = if self.line_drawing_active() && c.is_ascii() {
            Self::translate_line_drawing(c)
        } else {
            c
        };
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        // Zero-width chars (combining diacritics, ZWJ, VS16) must not advance
        // the cursor — otherwise every column after the first accent in any
        // non-ASCII output shifts right. Per-cell char buffers would be needed
        // to preserve the mark in the offline grid; the alignment fix is the
        // load-bearing change.
        if w == 0 {
            return;
        }
        // A wide character at the last column wraps to the next row in a real
        // terminal (DEC's "wide char never split"). Mirror that here.
        if self.col + w > self.cols {
            self.line_feed();
            self.col = 0;
        }
        self.last_char = Some(c);
        if self.row < self.rows && self.col < self.cols {
            self.cells[self.row][self.col] = (c, self.cur_style);
            // Mark the trailing cell of a wide char with a sentinel so column
            // bookkeeping stays right; `into_text` skips it when stringifying.
            if w == 2 && self.col + 1 < self.cols {
                self.cells[self.row][self.col + 1] = ('\0', self.cur_style);
            }
            if self.row > self.high_water {
                self.high_water = self.row;
            }
        }
        self.col += w;
    }

    fn line_feed(&mut self) {
        if self.row == self.scroll_bot {
            // Scroll the region up by one.
            let bg = self.bg_cell();
            for r in self.scroll_top..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![bg; self.cols];
            // Content was pushed into rows we had already touched; mark.
            if self.scroll_bot > self.high_water {
                self.high_water = self.scroll_bot;
            }
        } else if self.row + 1 < self.rows {
            self.row += 1;
            if self.row > self.high_water {
                self.high_water = self.row;
            }
        }
    }

    fn reverse_index(&mut self) {
        if self.row == self.scroll_top {
            let bg = self.bg_cell();
            for r in (self.scroll_top + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.scroll_top] = vec![bg; self.cols];
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    fn erase_below(&mut self) {
        if self.row < self.rows {
            let bg = self.bg_cell();
            for c in self.col..self.cols {
                self.cells[self.row][c] = bg;
            }
            for r in self.row + 1..self.rows {
                for c in 0..self.cols {
                    self.cells[r][c] = bg;
                }
            }
        }
    }

    fn erase_above(&mut self) {
        let bg = self.bg_cell();
        for r in 0..self.row {
            for c in 0..self.cols {
                self.cells[r][c] = bg;
            }
        }
        if self.row < self.rows {
            for c in 0..=self.col.min(self.cols - 1) {
                self.cells[self.row][c] = bg;
            }
        }
    }

    fn erase_all(&mut self) {
        let bg = self.bg_cell();
        for r in 0..self.rows {
            for c in 0..self.cols {
                self.cells[r][c] = bg;
            }
        }
        // Reset high-water: the screen is blank again. The cursor's row
        // will re-set it as soon as the program writes new content.
        self.high_water = 0;
    }

    fn erase_line_right(&mut self) {
        if self.row < self.rows {
            let bg = self.bg_cell();
            for c in self.col..self.cols {
                self.cells[self.row][c] = bg;
            }
        }
    }

    fn erase_line_left(&mut self) {
        if self.row < self.rows {
            let bg = self.bg_cell();
            for c in 0..=self.col.min(self.cols - 1) {
                self.cells[self.row][c] = bg;
            }
        }
    }

    fn erase_line(&mut self) {
        if self.row < self.rows {
            let bg = self.bg_cell();
            for c in 0..self.cols {
                self.cells[self.row][c] = bg;
            }
        }
    }

    /// ICH — insert `n` blank cells at the cursor, shifting the rest of the
    /// row right (off-screen cells fall off the end).
    fn insert_chars(&mut self, n: usize) {
        if self.row >= self.rows {
            return;
        }
        let n = n.min(self.cols.saturating_sub(self.col));
        if n == 0 {
            return;
        }
        let bg = self.bg_cell();
        let row = &mut self.cells[self.row];
        // Shift right from col..cols-n  →  col+n..cols.
        for c in (self.col + n..self.cols).rev() {
            row[c] = row[c - n];
        }
        for c in self.col..self.col + n {
            row[c] = bg;
        }
    }

    /// DCH — delete `n` cells at the cursor, shifting the rest of the row
    /// left and padding the right edge with the current background.
    fn delete_chars(&mut self, n: usize) {
        if self.row >= self.rows {
            return;
        }
        let n = n.min(self.cols.saturating_sub(self.col));
        if n == 0 {
            return;
        }
        let bg = self.bg_cell();
        let row = &mut self.cells[self.row];
        for c in self.col..self.cols.saturating_sub(n) {
            row[c] = row[c + n];
        }
        for c in self.cols.saturating_sub(n)..self.cols {
            row[c] = bg;
        }
    }

    /// ECH — erase `n` cells in place starting at cursor. Cursor unmoved,
    /// nothing shifts; cells become blanks (with current bg if BCE applies).
    fn erase_chars(&mut self, n: usize) {
        if self.row >= self.rows {
            return;
        }
        let bg = self.bg_cell();
        let end = (self.col + n).min(self.cols);
        for c in self.col..end {
            self.cells[self.row][c] = bg;
        }
    }

    /// REP — repeat the last printed char `n` times. Apps like `seq | column`
    /// or fancy progress bars use this to compress runs of the same glyph.
    fn repeat_last(&mut self, n: usize) {
        let Some(c) = self.last_char else { return };
        for _ in 0..n {
            self.put_char(c);
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.row + 1);
        let bg = self.bg_cell();
        for _ in 0..n {
            for r in (self.row + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.row] = vec![bg; self.cols];
        }
    }

    fn delete_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.row + 1);
        let bg = self.bg_cell();
        for _ in 0..n {
            for r in self.row..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![bg; self.cols];
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        let bg = self.bg_cell();
        for _ in 0..n {
            for r in self.scroll_top..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![bg; self.cols];
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        let bg = self.bg_cell();
        for _ in 0..n {
            for r in (self.scroll_top + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.scroll_top] = vec![bg; self.cols];
        }
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedState {
            row: self.row,
            col: self.col,
            scroll_top: self.scroll_top,
            scroll_bot: self.scroll_bot,
            cur_style: self.cur_style,
            cur_state: self.cur_state.clone(),
            g0_line_drawing: self.g0_line_drawing,
            g1_line_drawing: self.g1_line_drawing,
            gl_is_g1: self.gl_is_g1,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(s) = self.saved.clone() {
            self.row = s.row.min(self.rows - 1);
            self.col = s.col.min(self.cols - 1);
            self.scroll_top = s.scroll_top.min(self.rows - 1);
            self.scroll_bot = s.scroll_bot.min(self.rows - 1).max(self.scroll_top);
            self.cur_state = s.cur_state;
            self.cur_style = s.cur_style;
            self.g0_line_drawing = s.g0_line_drawing;
            self.g1_line_drawing = s.g1_line_drawing;
            self.gl_is_g1 = s.gl_is_g1;
        }
    }

    fn into_text(self) -> String {
        let last = self.high_water.min(self.rows - 1);
        let mut out = String::new();
        let mut cur_id: u16 = DEFAULT_STYLE;
        let mut cur_link: Option<String> = None;

        for r in 0..=last {
            // Trim trailing blanks-of-default-style — colored trailing cells
            // (e.g. a status bar that paints to end-of-line) must be kept.
            let line = &self.cells[r];
            let mut end = line.len();
            while end > 0 {
                let (c, sid) = line[end - 1];
                if (c == ' ' || c == '\0') && sid == DEFAULT_STYLE {
                    end -= 1;
                } else {
                    break;
                }
            }
            // Build a parallel char/style array, skipping wide-char continuation
            // sentinels so the bidi pass sees the logical text exactly once per
            // glyph (matching what the user sees / what BiDi rules assume).
            let mut chars: Vec<char> = Vec::with_capacity(end);
            let mut sids: Vec<u16> = Vec::with_capacity(end);
            for &(c, sid) in &line[..end] {
                if c == '\0' {
                    continue;
                }
                chars.push(c);
                sids.push(sid);
            }
            emit_bidi_line(
                &chars,
                &sids,
                &self.style_table,
                &mut out,
                &mut cur_id,
                &mut cur_link,
            );
            if r < last {
                out.push('\n');
            }
        }
        // Close any open style + hyperlink so the rendered block doesn't bleed
        // into following blocks.
        if cur_id != DEFAULT_STYLE {
            out.push_str("\x1b[0m");
        }
        if cur_link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        // Trim trailing blank lines.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }
}

/// Append one grid row to `out` in visual order. LTR-only rows take a fast
/// path; rows with any RTL character are reordered via UAX#9 and
/// directionally-mirrored brackets are flipped inside RTL runs. SGR / OSC 8
/// state is updated through the borrowed cursors so it persists across rows.
fn emit_bidi_line(
    chars: &[char],
    sids: &[u16],
    style_table: &[super::ansi::AnsiStyleState],
    out: &mut String,
    cur_id: &mut u16,
    cur_link: &mut Option<String>,
) {
    let emit_char =
        |c: char, sid: u16, out: &mut String, cur_id: &mut u16, cur_link: &mut Option<String>| {
            if sid != *cur_id {
                let style = &style_table[sid as usize];
                out.push_str(&super::ansi::encode_sgr(style));
                let new_link = style.hyperlink.as_deref();
                if new_link != cur_link.as_deref() {
                    match new_link {
                        Some(uri) => {
                            out.push_str("\x1b]8;;");
                            out.push_str(uri);
                            out.push_str("\x1b\\");
                            *cur_link = Some(uri.to_string());
                        }
                        None => {
                            out.push_str("\x1b]8;;\x1b\\");
                            *cur_link = None;
                        }
                    }
                }
                *cur_id = sid;
            }
            out.push(c);
        };

    // Fast path for ASCII-only / LTR-only rows — avoids building a String and
    // running the bidi algorithm for every line of a `git log` output.
    if !chars.iter().any(|c| needs_bidi(*c)) {
        for (i, &c) in chars.iter().enumerate() {
            emit_char(c, sids[i], out, cur_id, cur_link);
        }
        return;
    }

    use unicode_bidi::ParagraphBidiInfo;
    use unicode_bidi_mirroring::get_mirrored;

    // Build the logical text + a byte_offset → char_index map so we can look
    // up the style id of any char emitted by the visual-run walker.
    let mut text = String::with_capacity(chars.len());
    let mut byte_to_char: Vec<usize> = Vec::with_capacity(chars.len() * 2 + 1);
    for (ci, &c) in chars.iter().enumerate() {
        let start = text.len();
        text.push(c);
        for _ in start..text.len() {
            byte_to_char.push(ci);
        }
    }
    byte_to_char.push(chars.len());

    let info = ParagraphBidiInfo::new(&text, None);
    if !info.has_rtl() {
        for (i, &c) in chars.iter().enumerate() {
            emit_char(c, sids[i], out, cur_id, cur_link);
        }
        return;
    }
    let (levels, runs) = info.visual_runs(0..text.len());
    for run in runs {
        if run.start >= levels.len() {
            continue;
        }
        let is_rtl = levels[run.start].is_rtl();
        let slice = &text[run.clone()];
        if is_rtl {
            // Walk chars in reverse, mirroring paired-bracket / mirrored
            // characters per UAX#9 rule L4.
            for (rel, ch) in slice.char_indices().rev() {
                let ci = byte_to_char[run.start + rel];
                let glyph = get_mirrored(ch).unwrap_or(ch);
                emit_char(glyph, sids[ci], out, cur_id, cur_link);
            }
        } else {
            for (rel, ch) in slice.char_indices() {
                let ci = byte_to_char[run.start + rel];
                emit_char(ch, sids[ci], out, cur_id, cur_link);
            }
        }
    }
}

/// Cheap pre-check: any code point that the bidi algorithm could possibly
/// reorder. Avoids paying for `ParagraphBidiInfo::new` on every ASCII line.
/// The check is intentionally generous — false positives only cost one extra
/// bidi pass; false negatives would render RTL output in logical order.
fn needs_bidi(c: char) -> bool {
    let n = c as u32;
    // Hebrew, Arabic, Syriac, Thaana, NKo, Samaritan, Mandaic + the broader
    // RTL ranges, plus Arabic supplements / Hebrew presentation forms.
    (0x0590..=0x08FF).contains(&n)
        || (0xFB1D..=0xFDFF).contains(&n)
        || (0xFE70..=0xFEFF).contains(&n)
        // Supplementary RTL blocks (Imperial Aramaic … Mende Kikakui, etc.).
        || (0x10800..=0x10FFF).contains(&n)
        || (0x1E800..=0x1EFFF).contains(&n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_clear_and_cursor_moves() {
        assert!(has_cursor_positioning(b"\x1b[2J"));
        assert!(has_cursor_positioning(b"\x1b[H"));
        assert!(has_cursor_positioning(b"\x1b[5;1H"));
        assert!(has_cursor_positioning(b"\x1b[K"));
        assert!(has_cursor_positioning(b"abc\x1b[Adef"));
    }

    #[test]
    fn ignores_color_only_streams() {
        // SGR (color) escapes should NOT trigger the emulator — strip_ansi handles
        // them and the raw stream is already in correct text order.
        assert!(!has_cursor_positioning(b"\x1b[31mred\x1b[0m"));
        assert!(!has_cursor_positioning(b"plain text"));
        assert!(!has_cursor_positioning(b""));
    }

    #[test]
    fn collapses_less_x_repaints_to_final_page() {
        // Mimics `less -X` paging through `git log`: three pages, each preceded
        // by clear-screen + cursor-home. The final visible content is page 3.
        let bytes = b"\x1b[2J\x1b[H\
            commit AAAAAAAA (HEAD)\nAuthor: tester\nDate:   today\n\n    first commit\n:\
            \x1b[2J\x1b[H\
            commit AAAAAAAA (HEAD)\nAuthor: tester\nDate:   today\n\n    first commit\n:...skipping...\ncommit BBBBBBBB\nAuthor: tester\nDate:   yesterday\n\n    second commit\n:\
            \x1b[2J\x1b[H\
            commit BBBBBBBB\nAuthor: tester\nDate:   yesterday\n\n    second commit\n\ncommit CCCCCCCC\nAuthor: tester\nDate:   long ago\n\n    third commit\n(END)";
        let out = render_to_text(bytes, 80, 24);
        // Only the final page should remain.
        assert_eq!(out.matches("commit AAAAAAAA").count(), 0);
        assert_eq!(out.matches("commit BBBBBBBB").count(), 1);
        assert_eq!(out.matches("commit CCCCCCCC").count(), 1);
        assert!(out.contains("(END)"));
        // No "...skipping..." chrome leaks in from earlier pages.
        assert!(!out.contains("...skipping..."));
    }

    #[test]
    fn streams_plain_output_unchanged() {
        let out = render_to_text(b"line one\nline two\nline three", 80, 24);
        assert_eq!(out, "line one\nline two\nline three");
    }

    #[test]
    fn handles_carriage_return_overwrite() {
        // A progress bar overwrites itself with \r. Final state is "100%   ".
        let out = render_to_text(b"  0% loading\r 50% loading\r100% loading", 80, 24);
        assert_eq!(out, "100% loading");
    }

    #[test]
    fn cursor_address_writes_into_grid() {
        // Move cursor to (3, 5) and write "hi".
        let bytes = b"\x1b[3;5Hhi";
        let out = render_to_text(bytes, 80, 24);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "    hi");
    }

    #[test]
    fn colors_survive_cursor_positioning_replay() {
        // A clear-screen + home + colorized line. The recorded output must
        // contain an SGR sequence so the rendered block keeps its color
        // (previously stripped entirely by the no-SGR grid).
        let palette = [gtk4::gdk::RGBA::BLACK; 16];
        let bytes = b"\x1b[2J\x1b[H\x1b[31mred-text\x1b[0m plain";
        let out = render_to_ansi(bytes, 80, 24, &palette);
        assert!(out.contains("red-text"));
        assert!(out.contains("plain"));
        // The line carries an SGR escape and a reset.
        assert!(out.contains("\x1b["), "expected SGR in output: {out:?}");
    }

    #[test]
    fn combining_marks_do_not_shift_columns() {
        // "é" written as `e` + U+0301 (combining acute). Both chars must end
        // up in the same column so the trailing `|` lands at column 2, not 3.
        let bytes = "e\u{301}|".as_bytes();
        let out = render_to_text(bytes, 10, 1);
        // Output is now base-only (combining mark dropped from the offline
        // grid), but column alignment is preserved.
        assert_eq!(out, "e|");
    }

    #[test]
    fn ich_shifts_chars_right() {
        // Write "abcde", move cursor to col 1 (b), insert 2 blanks → "a  bcd"
        // (e fell off the end if we had a tiny grid, but with cols 80 it stays).
        let bytes = b"abcde\x1b[1;2H\x1b[2@";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "a  bcde");
    }

    #[test]
    fn dch_shifts_chars_left() {
        // Write "abcde", move to col 1 (b), delete 2 chars → "ade".
        let bytes = b"abcde\x1b[1;2H\x1b[2P";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "ade");
    }

    #[test]
    fn ech_erases_in_place() {
        // Write "abcde", move to col 1 (b), erase 2 chars → "a  de" (cursor unmoved).
        let bytes = b"abcde\x1b[1;2H\x1b[2X";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "a  de");
    }

    #[test]
    fn rep_repeats_last_char() {
        // "a" then REP 4 → "aaaaa".
        let bytes = b"a\x1b[4b";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "aaaaa");
    }

    #[test]
    fn sixel_payload_is_replaced_with_placeholder() {
        // \eP q ...payload... \e\  — sixel-style DCS. The binary body must not
        // leak into the rendered output; the placeholder takes its place.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"before \x1bPq");
        bytes.extend(std::iter::repeat(b'#').take(200));
        bytes.extend_from_slice(b"\x1b\\ after");
        let out = render_to_text(&bytes, 80, 24);
        assert!(!out.contains("##########"), "raw payload leaked: {out:?}");
        assert!(out.contains("[image]"), "placeholder missing: {out:?}");
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn kitty_apc_payload_is_replaced_with_placeholder() {
        // \e_G ...payload... \e\
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"x\x1b_G");
        bytes.extend(std::iter::repeat(b'a').take(50));
        bytes.extend_from_slice(b"\x1b\\y");
        let out = render_to_text(&bytes, 80, 24);
        assert!(!out.contains("aaaaaaaaaa"), "raw payload leaked: {out:?}");
        assert!(out.contains("[graphic]"));
        assert!(out.starts_with('x'));
        assert!(out.ends_with('y'));
    }

    #[test]
    fn osc8_hyperlinks_survive_replay() {
        // `\e]8;;https://x.example\e\\` link \e]8;;\e\\ plain
        // Replay must preserve both the link and the URI on the rendered text.
        let palette = [gtk4::gdk::RGBA::BLACK; 16];
        let bytes = b"\x1b[2J\x1b[H\x1b]8;;https://x.example\x1b\\link\x1b]8;;\x1b\\ plain";
        let out = render_to_ansi(bytes, 80, 24, &palette);
        assert!(out.contains("link"));
        assert!(out.contains("https://x.example"), "uri missing: {out:?}");
        // Both the opener and the closer were emitted.
        assert!(
            out.matches("\x1b]8;;").count() >= 2,
            "expected open+close: {out:?}"
        );
    }

    #[test]
    fn dec_line_drawing_translates_letters() {
        // ESC ( 0  selects DEC graphics for G0. Then "lqk" should render as
        // ┌─┐ (upper-left, horizontal, upper-right).
        let bytes = b"\x1b(0lqk\x1b(B";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "┌─┐");
    }

    #[test]
    fn line_clear_wipes_old_content() {
        // Write a long line, return, clear-line-to-end, write short — only short
        // remains.
        let bytes = b"old long content\rnew\x1b[K";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "new");
    }

    #[test]
    fn bidi_arabic_renders_rtl() {
        // Pure RTL line: Arabic letters BAA, MEEM, KAF (logical order).
        // Visually each glyph should be emitted in reverse logical order.
        let bytes = "\u{0628}\u{0645}\u{0643}".as_bytes(); // ب م ك
        let out = render_to_text(bytes, 80, 24);
        // Reordered visually right-to-left: KAF, MEEM, BAA
        assert_eq!(out, "\u{0643}\u{0645}\u{0628}");
    }

    #[test]
    fn bidi_mirrors_brackets_in_rtl_run() {
        // RTL run containing a paired bracket should flip to its mirror.
        // Hebrew alef, opening parenthesis, hebrew bet — visually the
        // parenthesis becomes ')'.
        let bytes = "\u{05D0}(\u{05D1}".as_bytes();
        let out = render_to_text(bytes, 80, 24);
        assert!(
            out.chars().any(|c| c == ')'),
            "expected mirrored bracket in {:?}",
            out
        );
        assert!(!out.contains('('));
    }

    #[test]
    fn bidi_ltr_only_lines_are_unchanged() {
        // Sanity: ASCII-only output goes through the fast path identical to
        // pre-bidi behavior.
        let out = render_to_text(b"hello world\n123 abc", 80, 24);
        assert_eq!(out, "hello world\n123 abc");
    }
}
