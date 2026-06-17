//! Pager-snapshot capture for alt-screen commands.
//!
//! When a command enters the alt-screen (`less`, `man`, `git log`, …) all of its
//! output is painted onto the alternate buffer and torn down on exit, so the
//! block view's output buffer would otherwise be empty. To leave a readable
//! block behind, we snapshot the visible VTE grid on every frame while in
//! alt-screen, then merge the pages by their scroll overlap when the app exits
//! and feed the result into the command's output buffer.
//!
//! Ported from jterm4's `block_view/alt_screen.rs` (pure helpers only).

use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{Terminal, TerminalExt};

/// Scrape the currently-visible VTE grid as plain text.
pub(crate) fn visible_vte_text(vte: &Terminal) -> String {
    let rows = vte.row_count();
    let cols = vte.column_count();
    if rows <= 0 || cols <= 0 {
        return String::new();
    }
    use gtk4::prelude::{AdjustmentExt, ScrollableExt};
    let adj = vte.vadjustment().expect("vte has vadjustment");
    let (av, al, au, ap) = (adj.value(), adj.lower(), adj.upper(), adj.page_size());
    let top = av.floor() as i64;
    let (text0, _) = vte.text_range_format(
        vte4::Format::Text,
        0,
        0,
        rows.saturating_sub(1),
        cols.saturating_sub(1),
    );
    let s0 = text0.map(|s| s.to_string()).unwrap_or_default();
    let (text1, _) = vte.text_range_format(
        vte4::Format::Text,
        top,
        0,
        top + rows.saturating_sub(1),
        cols.saturating_sub(1),
    );
    let s1 = text1.map(|s| s.to_string()).unwrap_or_default();
    eprintln!(
        "[altdbg] visible: adj(val={} low={} up={} page={}) top={} | s0={}ch nonblank={} | s1@top={}ch nonblank={}",
        av, al, au, ap, top,
        s0.len(), s0.chars().filter(|c| !c.is_whitespace()).count(),
        s1.len(), s1.chars().filter(|c| !c.is_whitespace()).count(),
    );
    if s0.chars().any(|c| !c.is_whitespace()) {
        s0
    } else {
        s1
    }
}

/// Pager chrome that should never appear in the recorded block.
pub(crate) fn is_pager_chrome_line(line: &str) -> bool {
    matches!(line.trim(), ":" | "(END)" | "END")
}

/// Trim a raw frame down to its meaningful content: drop trailing whitespace,
/// pager status lines, and leading/trailing blank rows. Returns `""` for frames
/// that are still mid-render (`...skipping...`).
pub(crate) fn normalize_pager_snapshot(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !is_pager_chrome_line(line))
        .collect();

    if lines.iter().any(|line| line.trim().contains("...skipping...")) {
        return String::new();
    }

    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());

    match (first, last) {
        (Some(start), Some(end)) if start <= end => lines[start..=end].join("\n"),
        _ => String::new(),
    }
}

/// Number of trailing lines of `existing` that equal the leading lines of `next`
/// (the scroll overlap between two consecutive pages).
pub(crate) fn overlap_line_count(existing: &[String], next: &[String]) -> usize {
    let max_overlap = existing.len().min(next.len());
    for count in (1..=max_overlap).rev() {
        if existing[existing.len() - count..] == next[..count]
            && (count > 1 || !existing[existing.len() - count].trim().is_empty())
        {
            return count;
        }
    }
    0
}

/// Stitch a sequence of pager frames into one document, dropping pages that are
/// a duplicate sub-window of what we already have and de-overlapping the rest.
pub(crate) fn merge_pager_snapshots(pages: Vec<String>) -> String {
    let mut merged: Vec<String> = Vec::new();

    for page in pages {
        let page_lines: Vec<String> = page.lines().map(|line| line.to_string()).collect();
        if page_lines.is_empty() {
            continue;
        }
        if merged.is_empty() {
            merged = page_lines;
            continue;
        }
        let first_line = &page_lines[0];
        if merged.iter().any(|line| line == first_line)
            && merged
                .windows(page_lines.len())
                .any(|window| window == page_lines.as_slice())
        {
            continue;
        }
        let overlap = overlap_line_count(&merged, &page_lines);
        merged.extend(page_lines.into_iter().skip(overlap));
    }

    merged.join("\n")
}

/// True if `bytes` contains a full clear-screen sequence (CSI 2J / CSI 3J).
/// Pagers that repaint a fresh page emit one; we snapshot the current frame
/// before it lands so paged-through content is not lost.
pub(crate) fn contains_clear_screen(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                params.push(bytes[i]);
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'J' && (params == b"2" || params == b"3") {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

/// Capture one frame: normalize, drop the pre-clear baseline (a stale render of
/// the previous command) and consecutive duplicates, then push.
pub(crate) fn record_pager_snapshot(
    vte: &Terminal,
    snapshots: &Rc<RefCell<Vec<String>>>,
    pre_clear: &Rc<RefCell<String>>,
) {
    let raw = visible_vte_text(vte);
    let snapshot = normalize_pager_snapshot(&raw);
    if snapshot.is_empty() {
        let sample: String = raw.chars().take(120).collect();
        eprintln!("[altdbg] record: EMPTY after normalize (raw {} chars, rows={} cols={}): {:?}",
            raw.len(), vte.row_count(), vte.column_count(), sample);
        return;
    }
    if pre_clear.borrow().as_str() == snapshot {
        eprintln!("[altdbg] record: SUPPRESSED (== pre_clear)");
        return;
    }
    let mut snapshots = snapshots.borrow_mut();
    if snapshots.last().map(|last| last == &snapshot).unwrap_or(false) {
        eprintln!("[altdbg] record: SUPPRESSED (dup of last)");
        return;
    }
    eprintln!("[altdbg] record: PUSH frame ({} lines):\n---\n{}\n---", snapshot.lines().count(), snapshot);
    snapshots.push(snapshot);
    pre_clear.borrow_mut().clear();
}

/// Defer a capture to the next idle tick so the VTE has finished painting the
/// frame. A generation token cancels captures scheduled before the last reset.
pub(crate) fn schedule_pager_snapshot(
    vte: &Terminal,
    snapshots: &Rc<RefCell<Vec<String>>>,
    generation: &Rc<Cell<u64>>,
    pre_clear: &Rc<RefCell<String>>,
) {
    let token = generation.get();
    let vte = vte.clone();
    let snapshots = snapshots.clone();
    let generation = generation.clone();
    let pre_clear = pre_clear.clone();
    glib::idle_add_local_once(move || {
        if generation.get() == token {
            record_pager_snapshot(&vte, &snapshots, &pre_clear);
        }
    });
}

/// Take all captured frames and return the merged document, clearing the buffer.
pub(crate) fn drain_pager_snapshots(snapshots: &Rc<RefCell<Vec<String>>>) -> String {
    let pages = std::mem::take(&mut *snapshots.borrow_mut());
    eprintln!("[altdbg] drain: {} pages", pages.len());
    let merged = merge_pager_snapshots(pages);
    eprintln!("[altdbg] drain: merged {} lines", merged.lines().count());
    merged
}

#[cfg(test)]
mod tests {
    use super::{contains_clear_screen, merge_pager_snapshots, normalize_pager_snapshot};

    #[test]
    fn detects_clear_screen_sequences() {
        assert!(contains_clear_screen(b"\x1b[2J"));
        assert!(contains_clear_screen(b"text\x1b[3Jmore"));
        assert!(contains_clear_screen(b"\x1b[H\x1b[2J"));
        assert!(!contains_clear_screen(b"\x1b[0J"));
        assert!(!contains_clear_screen(b"\x1b[1Jplain"));
        assert!(!contains_clear_screen(b"no escapes here"));
    }

    #[test]
    fn filters_less_status_lines() {
        let snapshot = normalize_pager_snapshot("\ncommit a\nAuthor: me\n:\n");
        assert_eq!(snapshot, "commit a\nAuthor: me");
    }

    #[test]
    fn merges_viewed_pages_by_overlap() {
        let merged = merge_pager_snapshots(vec![
            "commit a\nAuthor: me\nDate: today\n\n    first".to_string(),
            "Date: today\n\n    first\ncommit b\nAuthor: you".to_string(),
            "commit b\nAuthor: you\nDate: yesterday".to_string(),
        ]);
        assert_eq!(
            merged,
            "commit a\nAuthor: me\nDate: today\n\n    first\ncommit b\nAuthor: you\nDate: yesterday"
        );
    }

    #[test]
    fn skips_duplicate_pages() {
        let merged = merge_pager_snapshots(vec![
            "commit a\nAuthor: me".to_string(),
            "commit a\nAuthor: me".to_string(),
        ]);
        assert_eq!(merged, "commit a\nAuthor: me");
    }
}
