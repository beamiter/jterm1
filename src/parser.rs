//! OSC/CSI stream parser. Splits a raw PTY byte stream into semantic
//! `ParserEvent`s — passing through display bytes while extracting the OSC 133
//! shell-integration marks, OSC 7 cwd, OSC 52 clipboard, alt-screen toggles and
//! APC sequences that drive the block view. Ported from jterm4.

/// Events emitted by the stream parser.
#[derive(Debug, Clone)]
pub enum ParserEvent {
    /// Raw bytes that should be displayed verbatim (ANSI codes stripped of OSC 133/7).
    Bytes(Vec<u8>),
    /// OSC 133 ;A — prompt about to render.
    PromptStart,
    /// OSC 133 ;B — prompt finished, waiting for user input.
    PromptEnd,
    /// OSC 133 ;C — user pressed Enter, command is executing.
    CommandStart,
    /// OSC 133 ;D;<code> — command finished with exit code.
    CommandEnd(i32),
    /// OSC 7 — shell reported new CWD.
    CwdUpdate(String),
    /// CSI ? 1049 h — alt screen entered (vim, less, etc.)
    AltScreenEnter,
    /// CSI ? 1049 l — alt screen left.
    AltScreenLeave,
    /// OSC 52 — application set clipboard content.
    ClipboardSet(String),
    /// APC sequence (ESC _) — Kitty graphics protocol or similar.
    ApcSequence(Vec<u8>),
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// Saw ESC, waiting for next byte
    Esc,
    /// Inside CSI (ESC [): collecting parameter/intermediary bytes
    Csi { buf: Vec<u8> },
    /// Inside OSC (ESC ]): collecting bytes until ST (BEL or ESC \)
    Osc { buf: Vec<u8> },
    /// Just saw ESC while in OSC — next byte should be '\' for ST
    OscEsc { payload: Vec<u8> },
    /// Inside APC (ESC _): collecting bytes for Kitty graphics etc.
    Apc { buf: Vec<u8> },
    /// Saw ESC while in APC — next byte should be '\' for ST
    ApcEsc { payload: Vec<u8> },
    /// Inside DCS/PM — just consume until ST
    Ignore,
}

/// Which mouse-tracking mode the shell asked for. The active VTE in block-view
/// has no real PTY, so VTE never auto-generates mouse reports; the caller drives
/// reporting itself by reading this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// `?9` — only button presses (no release).
    X10,
    /// `?1000` — button press + release.
    Normal,
    /// `?1002` — press/release + motion while a button is held.
    ButtonEvent,
    /// `?1003` — press/release + all motion.
    AnyEvent,
}

/// Wire format for mouse reports. Set by `?1006`, `?1015`, `?1005` (or default
/// xterm encoding if none enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy `\e[M` + 3 bytes (button + 32, col + 32, row + 32).
    #[default]
    Default,
    /// `?1006` — SGR: `\e[<b;col;row;{M|m}`.
    Sgr,
    /// `?1015` — urxvt: `\e[b;col;row;M`.
    Urxvt,
    /// `?1005` — UTF-8 encoded coordinates.
    Utf8,
}

pub struct Parser {
    state: State,
    passthrough: Vec<u8>,
    /// `?2004` — shell asked for paste content to be bracketed with `\e[200~`
    /// / `\e[201~`. The caller wraps its own `Paste` write when this is on.
    bracketed_paste: bool,
    /// Which mouse mode is currently active (highest-priority "h" wins).
    mouse_mode: MouseMode,
    /// Active mouse encoding flags. SGR/Urxvt/Utf8 are toggled independently; a
    /// later "h" replaces the encoding choice.
    mouse_encoding: MouseEncoding,
    /// `?1004` — shell asked for `\e[I` / `\e[O` on focus enter/leave.
    focus_events: bool,
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            state: State::default(),
            passthrough: Vec::with_capacity(4096),
            bracketed_paste: false,
            mouse_mode: MouseMode::None,
            mouse_encoding: MouseEncoding::Default,
            focus_events: false,
        }
    }

    /// True while the shell has `?2004` enabled — callers should wrap pasted
    /// content with `\e[200~` / `\e[201~` before writing to the PTY.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Currently active mouse-tracking mode, or `None` when reporting is off.
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    /// Wire encoding the next mouse report should use.
    pub fn mouse_encoding(&self) -> MouseEncoding {
        self.mouse_encoding
    }

    /// True while `?1004` is enabled — callers should emit `\e[I` on focus-in,
    /// `\e[O` on focus-out.
    pub fn focus_events(&self) -> bool {
        self.focus_events
    }

    /// Apply each `?N` token from a `CSI ? Pm h/l` to the snooped state.
    /// `enable` = true for `h`, false for `l`. Unknown modes are ignored —
    /// they still pass through to the VTE.
    fn update_dec_private_modes(&mut self, params: &[u8], enable: bool) {
        for token in params.split(|&c| c == b';') {
            // Each token may itself start with `?` if the shell sent
            // `CSI ?1;?2 h`; tolerate that.
            let token = token.strip_prefix(b"?").unwrap_or(token);
            let n: u32 = match std::str::from_utf8(token).ok().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => continue,
            };
            match n {
                2004 => self.bracketed_paste = enable,
                9 => self.mouse_mode = if enable { MouseMode::X10 } else { MouseMode::None },
                1000 => self.mouse_mode = if enable { MouseMode::Normal } else { MouseMode::None },
                1002 => {
                    self.mouse_mode = if enable {
                        MouseMode::ButtonEvent
                    } else {
                        MouseMode::None
                    }
                }
                1003 => {
                    self.mouse_mode = if enable {
                        MouseMode::AnyEvent
                    } else {
                        MouseMode::None
                    }
                }
                1004 => self.focus_events = enable,
                1005 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Utf8
                    } else {
                        MouseEncoding::Default
                    }
                }
                1006 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Sgr
                    } else {
                        MouseEncoding::Default
                    }
                }
                1015 => {
                    self.mouse_encoding = if enable {
                        MouseEncoding::Urxvt
                    } else {
                        MouseEncoding::Default
                    }
                }
                _ => {}
            }
        }
    }

    pub fn feed(&mut self, data: &[u8], events: &mut Vec<ParserEvent>) {
        self.passthrough.clear();

        macro_rules! flush {
            () => {
                if !self.passthrough.is_empty() {
                    events.push(ParserEvent::Bytes(std::mem::take(&mut self.passthrough)));
                }
            };
        }

        // Ground-state fast-path: bulk-copy runs of bytes until the next ESC.
        // The previous per-byte loop dominated cost on heavy text streams; ESC
        // is the only byte that exits Ground, so memchr lets us hop directly
        // to the next state transition.
        let mut i = 0usize;
        let len = data.len();
        while i < len {
            if matches!(self.state, State::Ground) {
                match memchr::memchr(0x1b, &data[i..]) {
                    Some(off) => {
                        if off > 0 {
                            self.passthrough.extend_from_slice(&data[i..i + off]);
                        }
                        i += off + 1;
                        self.state = State::Esc;
                        continue;
                    }
                    None => {
                        self.passthrough.extend_from_slice(&data[i..]);
                        break;
                    }
                }
            }

            let b = data[i];
            i += 1;
            match &mut self.state {
                State::Ground => unreachable!("handled by fast-path above"),

                State::Esc => match b {
                    b'[' => {
                        // Do NOT emit "ESC[" yet. Buffer the whole CSI in state so a
                        // read boundary falling mid-sequence cannot split it across
                        // two Bytes events — downstream scanners (interactive-mode
                        // detection) rely on seeing each CSI whole.
                        self.state = State::Csi { buf: Vec::new() };
                    }
                    b']' => {
                        self.state = State::Osc { buf: Vec::new() };
                    }
                    b'_' => {
                        self.state = State::Apc { buf: Vec::new() };
                    }
                    b'P' | b'^' => {
                        self.state = State::Ignore;
                    }
                    _ => {
                        self.passthrough.push(0x1b);
                        self.passthrough.push(b);
                        self.state = State::Ground;
                    }
                },

                State::Csi { buf } => {
                    if (0x40..=0x7e).contains(&b) {
                        // Final byte of CSI sequence
                        let params = std::mem::take(buf);
                        self.state = State::Ground;
                        if b == b'h' && is_alt_screen_mode(&params) {
                            // Recognized alt-screen enter: drop the sequence bytes
                            // (never passed through) and emit the semantic event.
                            flush!();
                            events.push(ParserEvent::AltScreenEnter);
                        } else if b == b'l' && is_alt_screen_mode(&params) {
                            flush!();
                            events.push(ParserEvent::AltScreenLeave);
                        } else {
                            // Snoop DEC private mode set/reset for the modes the
                            // active VTE in block view cannot service for us
                            // (bracketed paste, mouse, focus). Still pass the
                            // CSI through verbatim so the VTE updates its own
                            // mirror state.
                            if (b == b'h' || b == b'l') && params.first() == Some(&b'?') {
                                self.update_dec_private_modes(&params[1..], b == b'h');
                            }
                            // Pass the complete sequence through as one contiguous run.
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                            self.passthrough.push(b);
                        }
                    } else {
                        buf.push(b);
                        // Guard against an unterminated CSI growing without bound
                        // (malformed stream). Dump what we have and recover.
                        if buf.len() > 4096 {
                            let params = std::mem::take(buf);
                            self.state = State::Ground;
                            self.passthrough.push(0x1b);
                            self.passthrough.push(b'[');
                            self.passthrough.extend_from_slice(&params);
                        }
                    }
                }

                State::Osc { buf } => match b {
                    0x07 => {
                        let payload = std::mem::take(buf);
                        self.state = State::Ground;
                        flush!();
                        handle_osc(&payload, events);
                    }
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::OscEsc { payload };
                    }
                    _ => {
                        buf.push(b);
                    }
                },

                State::OscEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    flush!();
                    handle_osc(&payload, events);
                    if b != b'\\' {
                        self.passthrough.push(b);
                    }
                }

                State::Apc { buf } => match b {
                    0x07 => {
                        let payload = std::mem::take(buf);
                        self.state = State::Ground;
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                    }
                    0x1b => {
                        let payload = std::mem::take(buf);
                        self.state = State::ApcEsc { payload };
                    }
                    _ => {
                        buf.push(b);
                    }
                },

                State::ApcEsc { payload } => {
                    let payload = std::mem::take(payload);
                    self.state = State::Ground;
                    if b == b'\\' {
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                    } else {
                        flush!();
                        events.push(ParserEvent::ApcSequence(payload));
                        self.passthrough.push(b);
                    }
                }

                State::Ignore => {
                    if b == 0x07 || b == 0x1b {
                        self.state = State::Ground;
                    }
                }
            }
        }

        flush!();
    }
}

fn handle_osc(payload: &[u8], events: &mut Vec<ParserEvent>) {
    let s = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => return,
    };

    // OSC 133 ; <mark> [; params...] — shell integration (FTCS).
    if let Some(rest) = s.strip_prefix("133;") {
        let mut fields = rest.split(';');
        match fields.next() {
            Some("A") => events.push(ParserEvent::PromptStart),
            Some("B") => events.push(ParserEvent::PromptEnd),
            Some("C") => events.push(ParserEvent::CommandStart),
            Some("D") => {
                let code = fields
                    .next()
                    .and_then(|f| f.parse::<i32>().ok())
                    .unwrap_or(0);
                events.push(ParserEvent::CommandEnd(code));
            }
            _ => {}
        }
        return;
    }

    // OSC 7 ; file://host/path — CWD update (path is percent-encoded per RFC 3986).
    if let Some(rest) = s.strip_prefix("7;") {
        let raw = if let Some(uri) = rest.strip_prefix("file://") {
            if let Some(idx) = uri.find('/') {
                &uri[idx..]
            } else {
                uri
            }
        } else {
            rest
        };
        let path = percent_decode(raw);
        if !path.is_empty() {
            events.push(ParserEvent::CwdUpdate(path));
        }
        return;
    }

    // OSC 52 ; <selection> ; <base64-data> — clipboard set
    if let Some(rest) = s.strip_prefix("52;") {
        if let Some(data_start) = rest.find(';') {
            let b64_data = &rest[data_start + 1..];
            if b64_data != "?" {
                if let Ok(decoded) = base64_decode(b64_data.as_bytes()) {
                    if let Ok(text) = String::from_utf8(decoded) {
                        events.push(ParserEvent::ClipboardSet(text));
                    }
                }
            }
        }
        return;
    }

    // All other OSC sequences: reconstruct and pass through
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.push(0x1b);
    bytes.push(b']');
    bytes.extend_from_slice(payload);
    bytes.push(0x07);
    events.push(ParserEvent::Bytes(bytes));
}

/// Percent-decode an OSC 7 path (e.g. "/home/me/My%20Docs" → "/home/me/My Docs").
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn base64_decode(input: &[u8]) -> Result<Vec<u8>, ()> {
    const TABLE: [u8; 256] = {
        let mut t = [0xFFu8; 256];
        let mut i = 0u8;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 26 {
                break;
            }
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        loop {
            if i >= 10 {
                break;
            }
            t[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'/' as usize] = 63;
        t
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input {
        if b == b'=' || b == b'\n' || b == b'\r' {
            continue;
        }
        let val = TABLE[b as usize];
        if val == 0xFF {
            return Err(());
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_bytes(events: &[ParserEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            if let ParserEvent::Bytes(b) = e {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn csi_not_split_across_feeds() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[3", &mut events);
        p.feed(b"1m", &mut events);
        let bytes_events: Vec<&Vec<u8>> = events
            .iter()
            .filter_map(|e| match e {
                ParserEvent::Bytes(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(bytes_events.len(), 1, "CSI must not be split into pieces");
        assert_eq!(bytes_events[0].as_slice(), b"\x1b[31m");
    }

    #[test]
    fn alt_screen_enter_leave_emitted_and_stripped() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b[?1049h\x1b[?1049l", &mut events);
        assert!(matches!(events[0], ParserEvent::AltScreenEnter));
        assert!(matches!(events[1], ParserEvent::AltScreenLeave));
        assert!(collect_bytes(&events).is_empty());
    }

    #[test]
    fn osc133_command_lifecycle() {
        let mut p = Parser::new();
        let mut events = Vec::new();
        p.feed(b"\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07", &mut events);
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                ParserEvent::PromptStart => "A",
                ParserEvent::CommandStart => "C",
                ParserEvent::CommandEnd(_) => "D",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, vec!["A", "C", "D"]);
    }
}
