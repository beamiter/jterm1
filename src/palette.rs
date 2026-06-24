//! Command palette: fuzzy-ranked search over multiple sources (actions, shell
//! history), with prefix-driven filters (`>` commands, `@` history).
//!
//! The UI lives in `dialogs::toggle_command_palette` — this module is the pure
//! data + ranking layer so it can be tested independently and reused by other
//! surfaces (e.g. the inline Ctrl-R popover in `dialogs::show_history_popover`).

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::path::Path;

use crate::keybindings::{Action, KeybindingMap};

/// Which sources the palette will draw from. The mode is the *default* — the
/// user can still narrow further with a prefix in the query text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteMode {
    /// Everything: actions + history.
    All,
    /// Only registered actions.
    Commands,
    /// Only shell history.
    History,
    /// `?` prefix: AI natural-language → shell command. The remaining text
    /// becomes the user prompt; gather returns a single "Ask AI" entry.
    Ai,
}

/// Parsed query: a mode (possibly tightened by a prefix) and the remaining
/// text used as the fuzzy needle.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    pub mode: PaletteMode,
    pub text: String,
}

impl Query {
    /// `>foo` forces command-only, `@foo` forces history-only, `?foo` forces
    /// AI natural-language → command. Otherwise the query inherits
    /// `default_mode`.
    pub fn parse(raw: &str, default_mode: PaletteMode) -> Self {
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix('>') {
            return Query { mode: PaletteMode::Commands, text: rest.trim_start().to_string() };
        }
        if let Some(rest) = trimmed.strip_prefix('@') {
            return Query { mode: PaletteMode::History, text: rest.trim_start().to_string() };
        }
        if let Some(rest) = trimmed.strip_prefix('?') {
            return Query { mode: PaletteMode::Ai, text: rest.trim_start().to_string() };
        }
        Query { mode: default_mode, text: trimmed.to_string() }
    }
}

/// What happens when the user activates an entry.
#[derive(Debug, Clone)]
pub(crate) enum Accept {
    /// Dispatch a built-in action.
    Action(Action),
    /// Type the command into the active pane without submitting (user can edit
    /// then press Enter). Safest default for history.
    TypeCommand(String),
    /// Forward the natural-language query to the AI bridge. The main loop
    /// fires the request, then types the returned command into the active
    /// pane (no autosubmit — same safety stance as TypeCommand).
    AskAi(String),
}

/// One row in the palette.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// Coarse priority bucket (lower = higher). Actions sit above history so
    /// "git" returns the binding for "Toggle git pane" before any past `git`
    /// invocations.
    pub tier: u8,
    /// Skim score, populated by [`gather`]. Higher = better.
    pub score: i64,
    pub label: String,
    pub sublabel: Option<String>,
    /// Right-aligned hint, e.g. the keybinding for an action or the cwd for a
    /// history entry.
    pub right: Option<String>,
    pub accept: Accept,
}

/// Read up to the last `max` records from a jsonl history file (newest last).
/// Records are pulled most-recent-first then reversed for display order.
pub(crate) fn read_history(path: &Path, max: usize) -> Vec<HistoryItem> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<HistoryItem> = text
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<HistoryItem>(line).ok())
        .filter(|h| !h.command.trim().is_empty())
        .take(max)
        .collect();
    // Deduplicate by command, keeping the most recent occurrence (which appears
    // first after the reverse). Preserves recency-ordering after dedup.
    let mut seen = std::collections::HashSet::new();
    out.retain(|h| seen.insert(h.command.clone()));
    out
}

/// Subset of `block::HistoryRecord` needed for the palette — kept minimal so we
/// stay forward-compatible with new fields in the on-disk format.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct HistoryItem {
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub end_time_ms: Option<u64>,
}

/// Run the query against all enabled sources, score, sort, and return up to
/// `limit` entries.
pub(crate) fn gather(
    query: &Query,
    kbmap: &KeybindingMap,
    history_path: Option<&Path>,
    limit: usize,
) -> Vec<Entry> {
    let matcher = SkimMatcherV2::default().smart_case();
    let mut out: Vec<Entry> = Vec::new();

    if matches!(query.mode, PaletteMode::All | PaletteMode::Commands) {
        for (action, binding) in kbmap.all_bound_actions() {
            let label = action.name().to_string();
            let entry = Entry {
                tier: 0,
                score: 0,
                label,
                sublabel: None,
                right: if binding.is_empty() { None } else { Some(binding) },
                accept: Accept::Action(action),
            };
            push_if_match(&matcher, &query.text, entry, &mut out);
        }
    }

    if matches!(query.mode, PaletteMode::Ai) {
        // Single synthetic entry: activating it kicks off the AI request.
        // We surface the raw user text in the label so they can see exactly
        // what's being sent. Empty query → harmless no-op entry that just
        // explains the prefix.
        let (label, sublabel, accept) = if query.text.trim().is_empty() {
            (
                "Type a natural-language request after ?".to_string(),
                Some("e.g. ? find files modified today".to_string()),
                Accept::TypeCommand(String::new()),
            )
        } else {
            (
                format!("Ask AI: {}", query.text),
                Some("Generates a shell command (review before running)".to_string()),
                Accept::AskAi(query.text.clone()),
            )
        };
        out.push(Entry {
            tier: 0,
            score: i64::MAX,
            label,
            sublabel,
            right: Some("?".to_string()),
            accept,
        });
        out.truncate(limit);
        return out;
    }

    if matches!(query.mode, PaletteMode::All | PaletteMode::History) {
        if let Some(path) = history_path {
            let items = read_history(path, 2000);
            // Recency boost: more-recent entries (lower index in `items`) get
            // a small score nudge so that with an empty query, history sorts
            // newest-first, and with a query the tie-breaker still favors
            // recent matches.
            let len = items.len();
            for (idx, item) in items.into_iter().enumerate() {
                let recency = (len - idx) as i64; // 1..=len
                let entry = Entry {
                    tier: 1,
                    score: recency,
                    label: item.command.clone(),
                    sublabel: Some(history_sublabel(&item)),
                    right: None,
                    accept: Accept::TypeCommand(item.command),
                };
                push_if_match(&matcher, &query.text, entry, &mut out);
            }
        }
    }

    out.sort_by(|a, b| a.tier.cmp(&b.tier).then(b.score.cmp(&a.score)));
    out.truncate(limit);
    out
}

fn push_if_match(matcher: &SkimMatcherV2, needle: &str, mut e: Entry, out: &mut Vec<Entry>) {
    if needle.is_empty() {
        out.push(e);
        return;
    }
    // Match against label first; fall back to sublabel for history entries
    // whose command is short but whose cwd narrows intent ("ls" in ~/proj/foo).
    let primary = matcher.fuzzy_match(&e.label, needle);
    let secondary = e.sublabel.as_deref().and_then(|s| matcher.fuzzy_match(s, needle));
    let score = match (primary, secondary) {
        (Some(p), Some(s)) => Some(p.max(s / 2)),
        (Some(p), None) => Some(p),
        (None, Some(s)) => Some(s / 2),
        (None, None) => None,
    };
    if let Some(s) = score {
        // Preserve the recency baseline as a tiny tie-breaker beneath the
        // fuzzy score so equally-good matches keep their recency order.
        e.score = s.saturating_mul(1000) + e.score;
        out.push(e);
    }
}

fn history_sublabel(item: &HistoryItem) -> String {
    let cwd = shorten_path(&item.cwd);
    if item.exit_code != 0 {
        format!("{cwd}  · exit {}", item.exit_code)
    } else {
        cwd
    }
}

fn shorten_path(p: &str) -> String {
    if p.is_empty() { return String::new(); }
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = p.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_overrides_default_mode() {
        let q = Query::parse(">tab", PaletteMode::History);
        assert_eq!(q.mode, PaletteMode::Commands);
        assert_eq!(q.text, "tab");

        let q = Query::parse("@git", PaletteMode::Commands);
        assert_eq!(q.mode, PaletteMode::History);
        assert_eq!(q.text, "git");

        let q = Query::parse("foo", PaletteMode::All);
        assert_eq!(q.mode, PaletteMode::All);
        assert_eq!(q.text, "foo");
    }

    #[test]
    fn empty_query_keeps_all_entries() {
        let kbmap = KeybindingMap::from_defaults();
        let entries = gather(
            &Query { mode: PaletteMode::Commands, text: String::new() },
            &kbmap,
            None,
            100,
        );
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e.tier == 0));
    }
}
