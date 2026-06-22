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
                        | b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G'
                        | b'd' | b'`'
                        | b'J' | b'K'
                        | b'r' | b's' | b'u'
                        | b'L' | b'M' | b'S' | b'T'
                ) {
                    return true;
                }
                i = j + 1;
            }
            b'M' => return true, // RI (reverse index) scrolls
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
                        // Skip OSC.
                        let mut j = i + 2;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                j += 1;
                                break;
                            }
                            if bytes[j] == 0x1b
                                && j + 1 < bytes.len()
                                && bytes[j + 1] == b'\\'
                            {
                                j += 2;
                                break;
                            }
                            j += 1;
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
                    b'(' | b')' | b'*' | b'+' => {
                        // Charset selection — skip the designator byte.
                        i += 3;
                    }
                    _ => i += 2,
                }
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
        let s = std::str::from_utf8(params).unwrap_or("");
        let parts: Vec<String> = if s.is_empty() {
            vec!["0".into()]
        } else {
            s.split(';').map(String::from).collect()
        };
        super::ansi::parse_sgr_params(&mut grid.cur_state, &parts, &grid.palette);
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

struct Grid {
    cells: Vec<Vec<Cell>>,
    cols: usize,
    rows: usize,
    row: usize,
    col: usize,
    saved: Option<(usize, usize)>,
    scroll_top: usize,
    scroll_bot: usize,
    /// Highest row index that has ever held non-blank content. The output
    /// trims to this so a 24-row grid receiving 5 lines doesn't pad with 19
    /// blanks.
    high_water: usize,
    /// Interned styles — cells store a u16 index, the table holds unique states.
    /// Index 0 is the default (no SGR), so plain output skips the table entirely.
    style_table: Vec<super::ansi::AnsiStyleState>,
    /// Currently-active SGR state (mutated by `m`-final CSI sequences).
    cur_state: super::ansi::AnsiStyleState,
    /// Interned index of `cur_state` — what new cells are written with.
    cur_style: u16,
    /// Palette used to resolve indexed SGR colors (30-37/40-47/90-97/100-107
    /// and 38;5/48;5) to RGB. Threaded in from the active terminal's theme.
    palette: [gtk4::gdk::RGBA; 16],
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
            cur_state: super::ansi::AnsiStyleState::default(),
            cur_style: DEFAULT_STYLE,
            palette,
        }
    }

    /// Find or insert `cur_state` in the style table and return its index.
    fn intern_style(&mut self) -> u16 {
        if self.cur_state == self.style_table[DEFAULT_STYLE as usize] {
            return DEFAULT_STYLE;
        }
        for (i, existing) in self.style_table.iter().enumerate() {
            if existing == &self.cur_state {
                return i as u16;
            }
        }
        // Cap to u16 to keep the table cheap; further unique styles all fold
        // onto the last slot (worst case is colored-output style mashing).
        if self.style_table.len() >= u16::MAX as usize {
            return (self.style_table.len() - 1) as u16;
        }
        self.style_table.push(self.cur_state.clone());
        (self.style_table.len() - 1) as u16
    }

    fn put_char(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
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
            for r in self.scroll_top..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![blank_cell(); self.cols];
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
            for r in (self.scroll_top + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.scroll_top] = vec![blank_cell(); self.cols];
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    fn erase_below(&mut self) {
        if self.row < self.rows {
            for c in self.col..self.cols {
                self.cells[self.row][c] = blank_cell();
            }
            for r in self.row + 1..self.rows {
                for c in 0..self.cols {
                    self.cells[r][c] = blank_cell();
                }
            }
        }
    }

    fn erase_above(&mut self) {
        for r in 0..self.row {
            for c in 0..self.cols {
                self.cells[r][c] = blank_cell();
            }
        }
        if self.row < self.rows {
            for c in 0..=self.col.min(self.cols - 1) {
                self.cells[self.row][c] = blank_cell();
            }
        }
    }

    fn erase_all(&mut self) {
        for r in 0..self.rows {
            for c in 0..self.cols {
                self.cells[r][c] = blank_cell();
            }
        }
        // Reset high-water: the screen is blank again. The cursor's row
        // will re-set it as soon as the program writes new content.
        self.high_water = 0;
    }

    fn erase_line_right(&mut self) {
        if self.row < self.rows {
            for c in self.col..self.cols {
                self.cells[self.row][c] = blank_cell();
            }
        }
    }

    fn erase_line_left(&mut self) {
        if self.row < self.rows {
            for c in 0..=self.col.min(self.cols - 1) {
                self.cells[self.row][c] = blank_cell();
            }
        }
    }

    fn erase_line(&mut self) {
        if self.row < self.rows {
            for c in 0..self.cols {
                self.cells[self.row][c] = blank_cell();
            }
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.row + 1);
        for _ in 0..n {
            for r in (self.row + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.row] = vec![blank_cell(); self.cols];
        }
    }

    fn delete_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.row + 1);
        for _ in 0..n {
            for r in self.row..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![blank_cell(); self.cols];
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        for _ in 0..n {
            for r in self.scroll_top..self.scroll_bot {
                self.cells[r] = std::mem::take(&mut self.cells[r + 1]);
            }
            self.cells[self.scroll_bot] = vec![blank_cell(); self.cols];
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        for _ in 0..n {
            for r in (self.scroll_top + 1..=self.scroll_bot).rev() {
                self.cells[r] = std::mem::take(&mut self.cells[r - 1]);
            }
            self.cells[self.scroll_top] = vec![blank_cell(); self.cols];
        }
    }

    fn save_cursor(&mut self) {
        self.saved = Some((self.row, self.col));
    }

    fn restore_cursor(&mut self) {
        if let Some((r, c)) = self.saved {
            self.row = r.min(self.rows - 1);
            self.col = c.min(self.cols - 1);
        }
    }

    fn into_text(self) -> String {
        let last = self.high_water.min(self.rows - 1);
        let mut out = String::new();
        let mut cur_id: u16 = DEFAULT_STYLE;

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
            for &(c, sid) in &line[..end] {
                if c == '\0' {
                    // wide-char continuation sentinel — already emitted as
                    // part of the previous char's glyph.
                    continue;
                }
                if sid != cur_id {
                    let style = &self.style_table[sid as usize];
                    out.push_str(&super::ansi::encode_sgr(style));
                    cur_id = sid;
                }
                out.push(c);
            }
            if r < last {
                out.push('\n');
            }
        }
        // Close any open style so the rendered block doesn't bleed SGR into
        // following blocks.
        if cur_id != DEFAULT_STYLE {
            out.push_str("\x1b[0m");
        }
        // Trim trailing blank lines.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }
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
    fn line_clear_wipes_old_content() {
        // Write a long line, return, clear-line-to-end, write short — only short
        // remains.
        let bytes = b"old long content\rnew\x1b[K";
        let out = render_to_text(bytes, 80, 24);
        assert_eq!(out, "new");
    }
}
