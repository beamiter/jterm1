//! Final-frame capture for alt-screen commands.
//!
//! When a command enters the alt-screen (`less`, `man`, `git log`, `top`, …)
//! all of its output is painted onto the alternate buffer and torn down on
//! exit. To leave a readable block behind we snapshot the alt grid exactly
//! once, on alt-screen leave, and feed that single frame into the command's
//! output buffer. Earlier versions scraped frames continuously and merged
//! them with overlap detection; that produced duplicated commits in `git log`
//! blocks (when the merge mis-aligned pages) and visible jitter in `top`/
//! `htop` (text_range_format racing the VTE's paint). Aligns with warp's
//! "alt-screen content is ephemeral; the live grid is what you see" model.

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
    let top = adj.value().floor() as i64;
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

    if lines
        .iter()
        .any(|line| line.trim().contains("...skipping..."))
    {
        return String::new();
    }

    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());

    match (first, last) {
        (Some(start), Some(end)) if start <= end => lines[start..=end].join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_pager_snapshot;

    #[test]
    fn filters_less_status_lines() {
        let snapshot = normalize_pager_snapshot("\ncommit a\nAuthor: me\n:\n");
        assert_eq!(snapshot, "commit a\nAuthor: me");
    }

    #[test]
    fn drops_mid_render_skipping_frames() {
        // `less` paints `...skipping...` on the next status line while it is
        // still emitting the new page; a snapshot taken at that instant is
        // partial and would otherwise pollute the recorded block.
        let snapshot =
            normalize_pager_snapshot("commit a\nAuthor: me\n:...skipping...\ncommit b\n");
        assert_eq!(snapshot, "");
    }
}
