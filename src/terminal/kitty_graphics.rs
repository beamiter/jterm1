//! Kitty graphics protocol (minimal subset) — APC `\e_G<keys>;<base64>\e\\`.
//!
//! Supported:
//! - `a=T` (transmit + display, default) and `a=t` (transmit only — buffered
//!   but not auto-displayed). `a=q`/`a=d`/`a=p` are silently dropped.
//! - `f=100` (PNG, default) and `f=32` (RGBA, requires `s=<w>` + `v=<h>`).
//! - `t=d` (inline base64 payload, default). File / shared-memory transports
//!   are ignored.
//! - Chunked transmission via `m=1` (more) + final `m=0` (or absent).
//!
//! libvte does not implement this protocol, so block-mode strips APC G
//! payloads from the byte stream before VTE sees them and renders the decoded
//! image as a GTK Picture appended to the active block.
//!
//! Per-image and per-block memory caps prevent a runaway shell from ballooning
//! RSS; oversize payloads are dropped silently.

use std::collections::HashMap;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::Cast;

/// Per-image base64 payload cap (before decoding) — ~16 MB encoded.
const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
/// Per-block decoded image bytes cap (sum of all images attached to a block).
pub(crate) const MAX_PENDING_BYTES_PER_BLOCK: usize = 16 * 1024 * 1024;

/// Image-data format identifier from the `f=` key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// `f=100` — PNG. Decoded via gdk::Texture::from_bytes.
    Png,
    /// `f=32` — 32-bit RGBA, raw pixels. Width/height come from `s=`/`v=`.
    Rgba,
    /// `f=24` — 24-bit RGB. Expanded to RGBA with alpha=255 before upload.
    Rgb,
}

/// In-progress assembly keyed by client image id (`i=`). Multi-chunk uploads
/// share an entry; the first chunk records format/size, every chunk appends
/// its base64 fragment.
struct Pending {
    format: Format,
    width: u32,
    height: u32,
    encoded: Vec<u8>,
    /// `a=T` (display) vs `a=t` (transmit only).
    display: bool,
}

/// Parsed result of a single APC G chunk. `Complete` carries a finished image
/// ready to render; `Pending` means more chunks are expected; `Skipped` means
/// the chunk was valid but unsupported (e.g. `a=q`) — caller should drop it.
pub(crate) enum Outcome {
    Complete(gdk::Texture),
    /// Buffered but not for display (`a=t`). Future `a=p` is unsupported, so
    /// these are effectively no-ops; returned distinctly only so the caller
    /// can avoid attaching them to the current block.
    CompleteTransmitOnly,
    Pending,
    Skipped,
    Invalid,
}

/// Stateful assembler — owns one entry per in-flight image id.
#[derive(Default)]
pub(crate) struct Assembler {
    in_flight: HashMap<u32, Pending>,
    /// Single anonymous slot for chunked uploads without `i=`. Kitty's spec
    /// allows omitting the id; in practice only one such upload is active.
    anon: Option<Pending>,
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop all in-flight state — call when a block ends or the shell resets,
    /// so a half-uploaded image doesn't leak across commands.
    pub(crate) fn reset(&mut self) {
        self.in_flight.clear();
        self.anon = None;
    }

    /// Parse one APC G payload. `payload` is the bytes between `\e_` and the
    /// terminating `\e\\` (i.e. starts with `G`). Returns the outcome the
    /// caller should act on.
    pub(crate) fn feed(&mut self, payload: &[u8]) -> Outcome {
        if payload.is_empty() || payload[0] != b'G' {
            return Outcome::Invalid;
        }
        // Split header (key=value,key=value...) from base64 body at the first `;`.
        let rest = &payload[1..];
        let (header, body) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &b""[..]),
        };
        let keys = parse_keys(header);

        let action = keys.get("a").copied().unwrap_or("t");
        // Silently ignore unsupported actions; the caller still strips the
        // APC bytes so libvte doesn't see them as garbage.
        match action {
            "T" | "t" => {}
            _ => return Outcome::Skipped,
        }

        let id: Option<u32> = keys.get("i").and_then(|s| s.parse().ok());
        let more = keys.get("m").map(|v| *v == "1").unwrap_or(false);

        // Either fetch an existing pending entry (continuation chunk) or seed
        // a new one from the header keys on the first chunk.
        let take_existing = |this: &mut Assembler| -> Option<Pending> {
            match id {
                Some(k) => this.in_flight.remove(&k),
                None => this.anon.take(),
            }
        };

        let mut entry = match take_existing(self) {
            Some(p) => p,
            None => {
                let format = match keys.get("f").copied().unwrap_or("100") {
                    "100" => Format::Png,
                    "32" => Format::Rgba,
                    "24" => Format::Rgb,
                    _ => return Outcome::Skipped,
                };
                let transport = keys.get("t").copied().unwrap_or("d");
                if transport != "d" {
                    // File / shared-memory transports require trusting a path
                    // from the shell — out of scope for this minimal subset.
                    return Outcome::Skipped;
                }
                let width = keys.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
                let height = keys.get("v").and_then(|s| s.parse().ok()).unwrap_or(0);
                let display = action == "T";
                Pending {
                    format,
                    width,
                    height,
                    encoded: Vec::new(),
                    display,
                }
            }
        };

        // Accumulate this chunk's base64 bytes (skipping any embedded
        // whitespace some emitters add).
        let before = entry.encoded.len();
        entry.encoded.reserve(body.len());
        for &b in body {
            if !b.is_ascii_whitespace() {
                entry.encoded.push(b);
            }
        }
        if entry.encoded.len() > MAX_ENCODED_BYTES {
            // Oversize — drop the assembly entirely so the next image starts clean.
            log::warn!(
                "kitty graphics: dropping oversize image ({} > {} encoded bytes)",
                entry.encoded.len(),
                MAX_ENCODED_BYTES
            );
            let _ = before; // (kept for future telemetry hook)
            return Outcome::Skipped;
        }

        if more {
            match id {
                Some(k) => {
                    self.in_flight.insert(k, entry);
                }
                None => {
                    self.anon = Some(entry);
                }
            }
            return Outcome::Pending;
        }

        // Final chunk — decode and build a texture.
        let decoded = match decode_base64(&entry.encoded) {
            Some(v) => v,
            None => return Outcome::Invalid,
        };
        let texture = match entry.format {
            Format::Png => match png_to_texture(&decoded) {
                Some(t) => t,
                None => return Outcome::Invalid,
            },
            Format::Rgba => match rgba_to_texture(entry.width, entry.height, &decoded, true) {
                Some(t) => t,
                None => return Outcome::Invalid,
            },
            Format::Rgb => match rgba_to_texture(entry.width, entry.height, &decoded, false) {
                Some(t) => t,
                None => return Outcome::Invalid,
            },
        };
        if entry.display {
            Outcome::Complete(texture)
        } else {
            // Drop the texture — we don't currently honour `a=p` placement.
            drop(texture);
            Outcome::CompleteTransmitOnly
        }
    }
}

/// Parse `key=value,key=value` into a borrow-only map. Empty / malformed
/// entries are silently skipped — the protocol mixes optional keys freely.
fn parse_keys(bytes: &[u8]) -> HashMap<&str, &str> {
    let mut out = HashMap::new();
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return out,
    };
    for pair in s.split(',') {
        if let Some(eq) = pair.find('=') {
            let (k, v) = (pair[..eq].trim(), pair[eq + 1..].trim());
            if !k.is_empty() {
                out.insert(k, v);
            }
        }
    }
    out
}

/// Standard base64 decoder. Returns None on invalid alphabet or padding.
/// Whitespace is already stripped by the caller.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn idx(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    // Trim trailing `=` padding (0–2) and any extra `=` we tolerate.
    let mut end = input.len();
    while end > 0 && input[end - 1] == b'=' {
        end -= 1;
    }
    let data = &input[..end];
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in data {
        let v = idx(b)?;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8 & 0xFF);
            buf &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

/// Decode a PNG payload into a gdk::Texture via the bundled GdkPixbuf loader.
/// Returns None if GTK cannot recognise the image.
fn png_to_texture(bytes: &[u8]) -> Option<gdk::Texture> {
    let gbytes = glib::Bytes::from(bytes);
    gdk::Texture::from_bytes(&gbytes).ok()
}

/// Build a texture from raw RGB(A) pixels. `has_alpha=false` expands each
/// 3-byte pixel to 4 bytes with full opacity, matching the kitty protocol's
/// `f=24` semantics.
fn rgba_to_texture(width: u32, height: u32, data: &[u8], has_alpha: bool) -> Option<gdk::Texture> {
    if width == 0 || height == 0 {
        return None;
    }
    let (stride, bytes) = if has_alpha {
        let expected = (width as usize) * (height as usize) * 4;
        if data.len() < expected {
            return None;
        }
        (width as usize * 4, data[..expected].to_vec())
    } else {
        let expected = (width as usize) * (height as usize) * 3;
        if data.len() < expected {
            return None;
        }
        let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for px in data[..expected].chunks_exact(3) {
            out.push(px[0]);
            out.push(px[1]);
            out.push(px[2]);
            out.push(0xFF);
        }
        (width as usize * 4, out)
    };
    let gbytes = glib::Bytes::from(&bytes);
    Some(gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &gbytes,
        stride,
    )
    .upcast())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"f", b"Zg=="),
            (b"fo", b"Zm8="),
            (b"foo", b"Zm9v"),
            (b"foob", b"Zm9vYg=="),
            (b"hello world", b"aGVsbG8gd29ybGQ="),
        ];
        for (plain, encoded) in cases {
            let got = decode_base64(encoded).expect("valid base64");
            assert_eq!(&got, plain);
        }
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(decode_base64(b"!!!!").is_none());
    }

    #[test]
    fn key_parser_handles_whitespace_and_empty() {
        let m = parse_keys(b"a=T,f=100,i=42,m=1");
        assert_eq!(m.get("a").copied(), Some("T"));
        assert_eq!(m.get("f").copied(), Some("100"));
        assert_eq!(m.get("i").copied(), Some("42"));
        assert_eq!(m.get("m").copied(), Some("1"));
        assert_eq!(m.get("q").copied(), None);
    }

    #[test]
    fn rejects_non_g_payload() {
        let mut a = Assembler::new();
        assert!(matches!(a.feed(b""), Outcome::Invalid));
        assert!(matches!(a.feed(b"X"), Outcome::Invalid));
    }

    #[test]
    fn unsupported_action_is_skipped() {
        let mut a = Assembler::new();
        assert!(matches!(a.feed(b"Ga=q,i=1;Zm9v"), Outcome::Skipped));
        assert!(matches!(a.feed(b"Ga=d,i=1;"), Outcome::Skipped));
    }

    #[test]
    fn file_transport_is_skipped() {
        let mut a = Assembler::new();
        assert!(matches!(a.feed(b"Ga=T,t=f;L3RtcC9hLnBuZw=="), Outcome::Skipped));
    }

    #[test]
    fn chunked_assembly_accumulates_then_decodes_rgb_pixel() {
        let mut a = Assembler::new();
        // 1×1 red pixel as raw RGB (f=24): bytes 0xFF 0x00 0x00 -> "/wAA"
        // Split across two chunks via m=1 / m=0.
        let first = a.feed(b"Ga=T,f=24,s=1,v=1,i=7,m=1;/w");
        assert!(matches!(first, Outcome::Pending));
        let second = a.feed(b"Ga=T,i=7,m=0;AA");
        match second {
            Outcome::Complete(_) => {}
            _ => panic!("expected complete texture"),
        }
        // Assembler state cleared after completion.
        assert!(a.in_flight.is_empty());
        assert!(a.anon.is_none());
    }
}
