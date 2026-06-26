//! ANSI SGR → GTK `TextTag` rendering for finished-block output.
//!
//! A streaming parser that walks output bytes, tracks SGR style state, and emits
//! styled text runs which are applied to a `TextBuffer` as colored/attributed
//! tags. `\r` overwrite is handled per-line (so progress bars collapse to their
//! final frame) without a full terminal grid. Ported/condensed from jterm4's
//! `block_view/ansi.rs`.

use gtk::prelude::*;
use gtk4::gdk::RGBA;
use gtk4::glib::translate::IntoGlib;
use gtk4::TextBuffer;
use relm4::gtk;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Default, PartialEq)]
pub struct AnsiStyleState {
    pub foreground: Option<RGBA>,
    pub background: Option<RGBA>,
    pub bold: bool,
    pub italic: bool,
    pub underline_style: UnderlineStyle,
    pub underline_color: Option<RGBA>,
    pub strikethrough: bool,
    pub dim: bool,
    pub reverse: bool,
    pub hidden: bool,
    pub overline: bool,
    pub blink: bool,
    pub hyperlink: Option<String>,
}

#[derive(Clone)]
pub struct AnsiTextRun {
    pub text: String,
    pub style: AnsiStyleState,
}

pub fn ansi256_to_rgb(idx: u8, palette: &[RGBA; 16]) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            let c = palette[idx as usize];
            (
                (c.red() * 255.0) as u8,
                (c.green() * 255.0) as u8,
                (c.blue() * 255.0) as u8,
            )
        }
        16..=231 => {
            let idx = idx - 16;
            let r = (idx / 36) * 51;
            let g = ((idx % 36) / 6) * 51;
            let b = (idx % 6) * 51;
            (r, g, b)
        }
        232..=255 => {
            let gray = 8 + (idx - 232) * 10;
            (gray, gray, gray)
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> RGBA {
    RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// Iterator over `;`-separated SGR parameter chunks, with a leading private
/// marker (`?`, `>`, `=`) stripped. Borrows from the input bytes so callers
/// don't pay an allocation per CSI.
pub(crate) struct SgrChunks<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> SgrChunks<'a> {
    pub(crate) fn new(mut params: &'a [u8]) -> Self {
        if matches!(params.first(), Some(&b'?') | Some(&b'>') | Some(&b'=')) {
            params = &params[1..];
        }
        SgrChunks {
            rest: params,
            done: false,
        }
    }
}

impl<'a> Iterator for SgrChunks<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        match memchr::memchr(b';', self.rest) {
            Some(i) => {
                let (head, tail) = self.rest.split_at(i);
                self.rest = &tail[1..];
                Some(head)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

#[inline]
pub(crate) fn bytes_to_u32(bytes: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in bytes {
        if b.is_ascii_digit() {
            acc = acc.saturating_mul(10).saturating_add((b - b'0') as u32);
        } else {
            return acc;
        }
    }
    acc
}

#[inline]
fn bytes_to_u8(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() {
        return None;
    }
    let v = bytes_to_u32(bytes);
    if v <= 255 {
        Some(v as u8)
    } else {
        None
    }
}

fn parse_colon_color_bytes(sub_parts: &[&[u8]], palette: &[RGBA; 16]) -> Option<RGBA> {
    let mode = bytes_to_u32(sub_parts.get(1)?);
    match mode {
        5 => {
            let idx = bytes_to_u8(sub_parts.get(2)?)?;
            let (r, g, b) = ansi256_to_rgb(idx, palette);
            Some(rgb(r, g, b))
        }
        2 => {
            let nums: Vec<u8> = sub_parts[2..]
                .iter()
                .filter_map(|s| bytes_to_u8(s))
                .collect();
            if nums.len() >= 3 {
                Some(rgb(
                    nums[nums.len() - 3],
                    nums[nums.len() - 2],
                    nums[nums.len() - 1],
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse SGR params from raw param bytes (semicolon-separated, with optional
/// colon-delimited sub-parameters). Avoids the per-CSI `Vec<String>` and
/// `String::parse::<u32>` round-trip the old `&[String]` signature forced.
pub fn parse_sgr_params(style: &mut AnsiStyleState, params: &[u8], palette: &[RGBA; 16]) {
    // Empty params == bare `\e[m` == reset.
    let chunks: SgrChunks = SgrChunks::new(params);
    let parts: Vec<&[u8]> = chunks.collect();
    let mut index = 0;
    while index < parts.len() {
        let part = parts[index];
        if part.contains(&b':') {
            let sub_parts: Vec<&[u8]> = part.split(|&b| b == b':').collect();
            let base = bytes_to_u32(sub_parts[0]);
            match base {
                4 => {
                    let sub = sub_parts.get(1).map(|s| bytes_to_u32(s)).unwrap_or(1);
                    style.underline_style = match sub {
                        0 => UnderlineStyle::None,
                        1 => UnderlineStyle::Single,
                        2 => UnderlineStyle::Double,
                        3 => UnderlineStyle::Curly,
                        4 => UnderlineStyle::Dotted,
                        5 => UnderlineStyle::Dashed,
                        _ => UnderlineStyle::Single,
                    };
                }
                38 | 48 | 58 => {
                    if let Some(color) = parse_colon_color_bytes(&sub_parts, palette) {
                        match base {
                            38 => style.foreground = Some(color),
                            48 => style.background = Some(color),
                            _ => style.underline_color = Some(color),
                        }
                    }
                }
                _ => {}
            }
            index += 1;
            continue;
        }

        let param = if part.is_empty() {
            0
        } else {
            bytes_to_u32(part)
        };

        match param {
            0 => {
                let link = style.hyperlink.take();
                *style = AnsiStyleState::default();
                style.hyperlink = link;
            }
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline_style = UnderlineStyle::Single,
            5 | 6 => style.blink = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            25 => style.blink = false,
            24 => {
                style.underline_style = UnderlineStyle::None;
                style.underline_color = None;
            }
            29 => style.strikethrough = false,
            30..=37 => {
                let (r, g, b) = ansi256_to_rgb((param - 30) as u8, palette);
                style.foreground = Some(rgb(r, g, b));
            }
            39 => style.foreground = None,
            40..=47 => {
                let (r, g, b) = ansi256_to_rgb((param - 40) as u8, palette);
                style.background = Some(rgb(r, g, b));
            }
            49 => style.background = None,
            90..=97 => {
                let (r, g, b) = ansi256_to_rgb((param - 90 + 8) as u8, palette);
                style.foreground = Some(rgb(r, g, b));
            }
            100..=107 => {
                let (r, g, b) = ansi256_to_rgb((param - 100 + 8) as u8, palette);
                style.background = Some(rgb(r, g, b));
            }
            38 | 48 => {
                let target = if param == 38 {
                    &mut style.foreground
                } else {
                    &mut style.background
                };
                if index + 2 < parts.len() && parts[index + 1] == b"5" {
                    if let Some(ci) = bytes_to_u8(parts[index + 2]) {
                        let (r, g, b) = ansi256_to_rgb(ci, palette);
                        *target = Some(rgb(r, g, b));
                    }
                    index += 2;
                } else if index + 4 < parts.len() && parts[index + 1] == b"2" {
                    if let (Some(r), Some(g), Some(b)) = (
                        bytes_to_u8(parts[index + 2]),
                        bytes_to_u8(parts[index + 3]),
                        bytes_to_u8(parts[index + 4]),
                    ) {
                        *target = Some(rgb(r, g, b));
                    }
                    index += 4;
                }
            }
            58 => {
                if index + 2 < parts.len() && parts[index + 1] == b"5" {
                    if let Some(ci) = bytes_to_u8(parts[index + 2]) {
                        let (r, g, b) = ansi256_to_rgb(ci, palette);
                        style.underline_color = Some(rgb(r, g, b));
                    }
                    index += 2;
                } else if index + 4 < parts.len() && parts[index + 1] == b"2" {
                    if let (Some(r), Some(g), Some(b)) = (
                        bytes_to_u8(parts[index + 2]),
                        bytes_to_u8(parts[index + 3]),
                        bytes_to_u8(parts[index + 4]),
                    ) {
                        style.underline_color = Some(rgb(r, g, b));
                    }
                    index += 4;
                }
            }
            59 => style.underline_color = None,
            7 => style.reverse = true,
            8 => style.hidden = true,
            27 => style.reverse = false,
            28 => style.hidden = false,
            53 => style.overline = true,
            55 => style.overline = false,
            _ => {}
        }
        index += 1;
    }
}

fn ansi_tag_name(style: &AnsiStyleState) -> Option<String> {
    if style.foreground.is_none()
        && style.background.is_none()
        && !style.bold
        && !style.italic
        && style.underline_style == UnderlineStyle::None
        && style.underline_color.is_none()
        && !style.strikethrough
        && !style.dim
        && !style.reverse
        && !style.hidden
        && !style.overline
        && !style.blink
        && style.hyperlink.is_none()
    {
        return None;
    }
    let rgba_key = |color: Option<&RGBA>| match color {
        Some(c) => format!(
            "{:03}-{:03}-{:03}-{:03}",
            (c.red() * 255.0).round() as u8,
            (c.green() * 255.0).round() as u8,
            (c.blue() * 255.0).round() as u8,
            (c.alpha() * 255.0).round() as u8,
        ),
        None => "none".to_string(),
    };
    let ul = style.underline_style as u8;
    let link_key = match &style.hyperlink {
        Some(uri) => {
            let mut h: u64 = 0;
            for b in uri.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u64);
            }
            format!("{h:016x}")
        }
        None => "none".to_string(),
    };
    Some(format!(
        "ansi-fg:{}-bg:{}-b{}-i{}-u{}-uc:{}-s{}-d{}-rv{}-hd{}-ov{}-bl{}-lk:{}",
        rgba_key(style.foreground.as_ref()),
        rgba_key(style.background.as_ref()),
        style.bold as u8,
        style.italic as u8,
        ul,
        rgba_key(style.underline_color.as_ref()),
        style.strikethrough as u8,
        style.dim as u8,
        style.reverse as u8,
        style.hidden as u8,
        style.overline as u8,
        style.blink as u8,
        link_key,
    ))
}

fn ensure_ansi_text_tag(buffer: &TextBuffer, style: &AnsiStyleState) -> Option<gtk::TextTag> {
    let tag_name = ansi_tag_name(style)?;
    let tag_table = buffer.tag_table();
    if let Some(tag) = tag_table.lookup(&tag_name) {
        return Some(tag);
    }
    let tag = gtk::TextTag::new(Some(&tag_name));
    let (eff_fg, eff_bg) = if style.reverse {
        (style.background, style.foreground)
    } else {
        (style.foreground, style.background)
    };
    if let Some(mut fg) = eff_fg {
        if style.dim {
            fg.set_alpha(0.7);
        }
        tag.set_foreground_rgba(Some(&fg));
    }
    if style.hyperlink.is_some() && eff_fg.is_none() {
        tag.set_foreground_rgba(Some(&RGBA::new(0.4, 0.6, 1.0, 1.0)));
    }
    if let Some(bg) = eff_bg {
        tag.set_background_rgba(Some(&bg));
    }
    if style.hidden {
        tag.set_foreground_rgba(Some(&RGBA::new(0.0, 0.0, 0.0, 0.0)));
    }
    if style.overline {
        tag.set_overline(gtk::pango::Overline::Single);
    }
    if style.bold {
        tag.set_weight(gtk::pango::Weight::Bold.into_glib());
    }
    if style.italic {
        tag.set_style(gtk::pango::Style::Italic);
    }
    match style.underline_style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => tag.set_underline(gtk::pango::Underline::Single),
        UnderlineStyle::Double => tag.set_underline(gtk::pango::Underline::Double),
        UnderlineStyle::Curly => tag.set_underline(gtk::pango::Underline::Error),
        UnderlineStyle::Dotted | UnderlineStyle::Dashed => {
            tag.set_underline(gtk::pango::Underline::Single);
        }
    }
    if style.hyperlink.is_some() && style.underline_style == UnderlineStyle::None {
        tag.set_underline(gtk::pango::Underline::Single);
    }
    if let Some(uc) = style.underline_color {
        tag.set_underline_rgba(Some(&uc));
    }
    if style.strikethrough {
        tag.set_strikethrough(true);
    }
    if style.blink {
        // GTK/Pango has no animated blink; mirror what VTE's "Allow Blink: off"
        // does and just hint the attribute with mild emphasis (italic + reduced
        // alpha) so the user can see the cell was tagged.
        tag.set_style(gtk::pango::Style::Italic);
        if let Some(mut fg) = style.foreground {
            fg.set_alpha(fg.alpha() * 0.85);
            tag.set_foreground_rgba(Some(&fg));
        }
    }
    tag_table.add(&tag);
    Some(tag)
}

fn set_cell(line: &mut Vec<(char, AnsiStyleState)>, col: usize, c: char, style: &AnsiStyleState) {
    if col < line.len() {
        line[col] = (c, style.clone());
    } else {
        while line.len() < col {
            line.push((' ', AnsiStyleState::default()));
        }
        line.push((c, style.clone()));
    }
}

fn flush_line(runs: &mut Vec<AnsiTextRun>, line: &mut Vec<(char, AnsiStyleState)>) {
    if !line.is_empty() {
        let mut cur_style = line[0].1.clone();
        let mut cur_text = String::new();
        for (c, st) in line.iter() {
            if *st != cur_style {
                if !cur_text.is_empty() {
                    runs.push(AnsiTextRun {
                        text: std::mem::take(&mut cur_text),
                        style: cur_style.clone(),
                    });
                }
                cur_style = st.clone();
            }
            cur_text.push(*c);
        }
        if !cur_text.is_empty() {
            runs.push(AnsiTextRun {
                text: cur_text,
                style: cur_style,
            });
        }
        line.clear();
    }
}

/// Parse ANSI text into styled runs. The concatenation of `run.text` is the
/// plain text; offsets line up with `apply_ansi_runs_to_buffer`.
pub fn ansi_text_runs(input: &str, palette: &[RGBA; 16]) -> Vec<AnsiTextRun> {
    let chars: Vec<char> = input.chars().collect();
    let mut runs: Vec<AnsiTextRun> = Vec::new();
    let mut style = AnsiStyleState::default();
    let mut line: Vec<(char, AnsiStyleState)> = Vec::new();
    let mut col = 0usize;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\x1b' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                match chars[i] {
                    '[' => {
                        i += 1;
                        let start = i;
                        while i < chars.len() && !('@'..='~').contains(&chars[i]) {
                            i += 1;
                        }
                        if i < chars.len() {
                            let final_c = chars[i];
                            let params: String = chars[start..i].iter().collect();
                            i += 1;
                            match final_c {
                                'm' => {
                                    let bytes = params.as_bytes();
                                    if bytes.is_empty() {
                                        parse_sgr_params(&mut style, b"0", palette);
                                    } else {
                                        parse_sgr_params(&mut style, bytes, palette);
                                    }
                                }
                                'K' => {
                                    let n = params.parse::<u32>().unwrap_or(0);
                                    match n {
                                        0 => line.truncate(col),
                                        2 => line.clear(),
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    ']' => {
                        i += 1;
                        let mut payload = String::new();
                        while i < chars.len() {
                            if chars[i] == '\x07' {
                                i += 1;
                                break;
                            }
                            if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            payload.push(chars[i]);
                            i += 1;
                        }
                        if let Some(rest) = payload.strip_prefix("8;") {
                            if let Some(semi) = rest.find(';') {
                                let uri = &rest[semi + 1..];
                                style.hyperlink = if uri.is_empty() {
                                    None
                                } else {
                                    Some(uri.to_string())
                                };
                            }
                        }
                    }
                    '(' | ')' => {
                        i += 1;
                        if i < chars.len() {
                            i += 1;
                        }
                    }
                    _ => i += 1,
                }
            }
            '\n' => {
                flush_line(&mut runs, &mut line);
                runs.push(AnsiTextRun {
                    text: "\n".to_string(),
                    style: AnsiStyleState::default(),
                });
                col = 0;
                i += 1;
            }
            '\r' => {
                col = 0;
                i += 1;
            }
            '\t' => {
                let next = ((col / 8) + 1) * 8;
                while col < next {
                    set_cell(&mut line, col, ' ', &style);
                    col += 1;
                }
                i += 1;
            }
            '\x08' => {
                col = col.saturating_sub(1);
                i += 1;
            }
            c if (c as u32) < 0x20 => i += 1,
            c => {
                set_cell(&mut line, col, c, &style);
                col += 1;
                i += 1;
            }
        }
    }
    flush_line(&mut runs, &mut line);
    runs
}

fn ensure_osc8_tag(buffer: &TextBuffer, uri: &str) -> gtk::TextTag {
    let name = format!("osc8-link:{uri}");
    let tag_table = buffer.tag_table();
    if let Some(tag) = tag_table.lookup(&name) {
        return tag;
    }
    let tag = gtk::TextTag::new(Some(&name));
    tag_table.add(&tag);
    tag
}

pub fn apply_ansi_runs_to_buffer(buffer: &TextBuffer, start_offset: usize, runs: &[AnsiTextRun]) {
    let mut offset = start_offset;
    for run in runs {
        let len = run.text.chars().count();
        if len == 0 {
            continue;
        }
        let s = buffer.iter_at_offset(offset as i32);
        let e = buffer.iter_at_offset((offset + len) as i32);
        if let Some(tag) = ensure_ansi_text_tag(buffer, &run.style) {
            buffer.apply_tag(&tag, &s, &e);
        }
        if let Some(uri) = &run.style.hyperlink {
            let tag = ensure_osc8_tag(buffer, uri);
            buffer.apply_tag(&tag, &s, &e);
        }
        offset += len;
    }
}

/// Encode an `AnsiStyleState` back into a CSI SGR sequence such that feeding
/// the result through `ansi_text_runs` reproduces the same style. Used by
/// `grid.rs` to keep colors and attributes alive across the offline cursor-
/// positioning replay — without this, colorized pager output (`less` with
/// `LESS=R`, `git log --color`, `top`) loses all its color when the recorded
/// block is rendered. Always begins with `0` (reset) so it's standalone.
pub fn encode_sgr(style: &AnsiStyleState) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(32);
    s.push_str("\x1b[0");
    if style.bold {
        s.push_str(";1");
    }
    if style.dim {
        s.push_str(";2");
    }
    if style.italic {
        s.push_str(";3");
    }
    match style.underline_style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => s.push_str(";4"),
        UnderlineStyle::Double => s.push_str(";21"),
        UnderlineStyle::Curly => s.push_str(";4:3"),
        UnderlineStyle::Dotted => s.push_str(";4:4"),
        UnderlineStyle::Dashed => s.push_str(";4:5"),
    }
    if style.blink {
        s.push_str(";5");
    }
    if style.reverse {
        s.push_str(";7");
    }
    if style.hidden {
        s.push_str(";8");
    }
    if style.strikethrough {
        s.push_str(";9");
    }
    if style.overline {
        s.push_str(";53");
    }
    let push_rgb = |s: &mut String, lead: &str, c: &RGBA| {
        let _ = write!(
            s,
            ";{lead};2;{};{};{}",
            (c.red() * 255.0) as u8,
            (c.green() * 255.0) as u8,
            (c.blue() * 255.0) as u8,
        );
    };
    if let Some(c) = style.foreground.as_ref() {
        push_rgb(&mut s, "38", c);
    }
    if let Some(c) = style.background.as_ref() {
        push_rgb(&mut s, "48", c);
    }
    if let Some(c) = style.underline_color.as_ref() {
        push_rgb(&mut s, "58", c);
    }
    // hyperlink is OSC 8, not SGR; encoded separately if needed.
    s.push('m');
    s
}

/// Truncate a run list to at most `max_chars` characters.
pub fn truncate_runs(runs: &[AnsiTextRun], max_chars: usize) -> Vec<AnsiTextRun> {
    let mut out = Vec::new();
    let mut count = 0;
    for r in runs {
        let len = r.text.chars().count();
        if count + len <= max_chars {
            out.push(r.clone());
            count += len;
        } else {
            let take = max_chars - count;
            let text: String = r.text.chars().take(take).collect();
            if !text.is_empty() {
                out.push(AnsiTextRun {
                    text,
                    style: r.style.clone(),
                });
            }
            break;
        }
    }
    out
}

/// Char offset just past the `n`th newline (i.e. end of the first `n` lines).
pub fn char_offset_after_lines(runs: &[AnsiTextRun], n: usize) -> usize {
    let mut lines_seen = 0;
    let mut chars = 0;
    for r in runs {
        for c in r.text.chars() {
            chars += 1;
            if c == '\n' {
                lines_seen += 1;
                if lines_seen == n {
                    return chars;
                }
            }
        }
    }
    chars
}

/// Total newline count across all runs.
pub fn count_lines(runs: &[AnsiTextRun]) -> usize {
    runs.iter()
        .flat_map(|r| r.text.chars())
        .filter(|&c| c == '\n')
        .count()
}
