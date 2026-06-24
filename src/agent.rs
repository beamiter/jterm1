//! Agent mode — multi-turn LLM that proposes shell commands, watches their
//! output, and iterates. Inspired by Warp 2.x's agent.
//!
//! ## Safety model (immutable, by design)
//!
//! 1. **Per-command approval.** No "yes to all" / "yolo mode". Every command
//!    the model proposes shows up as an *Approve & Run* card that the user
//!    must click — including obvious commands like `ls`. This is the price
//!    of letting an LLM touch a real terminal. Users who want unattended
//!    execution can write a shell script.
//! 2. **Dangerous-command flagging.** A small regex blacklist (rm -rf /,
//!    mkfs.*, dd of=/dev/*, fork bomb, curl|sh) flips the Approve button
//!    to a destructive style and prefixes a `⚠ destructive` chip. Users
//!    can still approve — we just slow them down with a colour change so
//!    a stray Enter doesn't nuke their disk.
//! 3. **Single concurrent session.** AppModel holds at most one
//!    `AgentSession` at a time. Opening a second panel closes the first.
//! 4. **Turn cap.** `agent_max_turns` (default 20) bounds runaway loops.
//! 5. **Transcript byte cap.** Before sending to the LLM, the transcript is
//!    head+tail elided to `MAX_TRANSCRIPT_BYTES` so a chatty session can't
//!    OOM the prompt.
//! 6. **Output sample bound.** Each observation feeds the model at most
//!    `MAX_OBS_BYTES` of captured output (head+tail).
//! 7. **Cancel on close.** Closing the dialog calls `AgentSession::cancel`,
//!    which both flips the cancelled flag (suppressing pending LLM
//!    callbacks) and clears `awaiting_output` so a late block-finished
//!    event won't attach to a dead session.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use relm4::adw;
use relm4::gtk;
use relm4::ComponentSender;
use adw::prelude::*;

/// Hard cap on transcript bytes sent to the LLM. Past this, the middle is
/// elided. Chosen well below typical 100k context windows so the system
/// prompt + few-shots still fit comfortably alongside.
const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024;
/// Per-observation output sample cap (head+tail). Keeps the model from
/// drowning in a `find /` dump.
const MAX_OBS_BYTES: usize = 4 * 1024;

/// A single entry in the agent's running transcript. The conversation is
/// reconstructed from this list every turn (we don't cache server-side
/// chat history because the API contract varies between providers and
/// resending is fine for short sessions).
#[derive(Debug, Clone)]
pub(crate) enum Turn {
    /// User's free-text input. The first turn is always a User.
    User(String),
    /// The model's chain-of-thought sentence — surfaced dimly in the UI so
    /// the user can see *why* a command was proposed. Optional per turn.
    AssistantThought(String),
    /// The model's chat response that does NOT propose a command. Used for
    /// clarifying questions, summaries, and the final "done" answer.
    AssistantSay(String),
    /// The model proposed a command. `approved` tracks user verdict:
    /// `None` = pending, `Some(true)` = ran (Observation follows),
    /// `Some(false)` = rejected.
    AssistantProposed {
        cmd: String,
        approved: Option<bool>,
    },
    /// The captured outcome of an approved command. `output_sample` is
    /// already truncated to `MAX_OBS_BYTES`.
    Observation {
        exit: i32,
        output_sample: String,
    },
}

impl Turn {
    /// Approximate byte size used for transcript-cap eviction.
    fn size(&self) -> usize {
        match self {
            Turn::User(s) | Turn::AssistantThought(s) | Turn::AssistantSay(s) => s.len() + 8,
            Turn::AssistantProposed { cmd, .. } => cmd.len() + 16,
            Turn::Observation { output_sample, .. } => output_sample.len() + 16,
        }
    }

    /// Render this turn for the LLM prompt. Format is plain text with
    /// `User:` / `Assistant:` / `Output:` markers — matches the few-shot
    /// examples in `build_agent_system_prompt`.
    fn to_prompt_line(&self) -> String {
        match self {
            Turn::User(s) => format!("User: {s}"),
            Turn::AssistantThought(s) => format!("Assistant (thought): {s}"),
            Turn::AssistantSay(s) => {
                // Wrap as a `say` action so the model sees its own format.
                let payload = serde_json::json!({"action": "say", "message": s});
                format!("Assistant: {payload}")
            }
            Turn::AssistantProposed { cmd, approved } => {
                let payload = serde_json::json!({"action": "run", "command": cmd});
                match approved {
                    None => format!("Assistant: {payload}"),
                    Some(true) => format!("Assistant: {payload}\n[user approved & ran]"),
                    Some(false) => format!("Assistant: {payload}\n[user rejected]"),
                }
            }
            Turn::Observation { exit, output_sample } => {
                format!("Output (exit={exit}):\n{output_sample}")
            }
        }
    }
}

/// The model's reply, parsed from JSON. Falls back to `Say(raw_text)` when
/// the JSON is malformed so the session stays usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedAction {
    Run { thought: Option<String>, command: String },
    Say { thought: Option<String>, message: String },
    Done { thought: Option<String>, message: String },
}

/// Best-effort JSON parser. Strips a markdown fence if the model wrapped
/// its reply, then validates required fields. Returns `Say(raw)` on any
/// failure — never errors, because surfacing the raw text in the panel
/// is more useful than a parse-error toast.
pub(crate) fn parse_action(raw: &str) -> ParsedAction {
    let trimmed = strip_fences(raw.trim()).trim();
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return ParsedAction::Say {
                thought: None,
                message: raw.trim().to_string(),
            };
        }
    };
    let thought = value
        .get("thought")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let action = value.get("action").and_then(|a| a.as_str()).unwrap_or("");
    match action {
        "run" => {
            let cmd = value
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if cmd.is_empty() {
                ParsedAction::Say {
                    thought,
                    message: raw.trim().to_string(),
                }
            } else {
                ParsedAction::Run { thought, command: cmd }
            }
        }
        "done" => ParsedAction::Done {
            thought,
            message: value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        },
        // "say" or anything unrecognised → treat as say so unknown action
        // names don't drop the model's reply on the floor.
        _ => ParsedAction::Say {
            thought,
            message: value
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| raw.trim().to_string()),
        },
    }
}

fn strip_fences(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```json") {
        let after = rest.trim_start_matches('\n');
        if let Some(inner) = after.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        let after = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        if let Some(inner) = after.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    s
}

/// Match a command against the destructive-pattern blacklist. Returns a
/// short human-readable reason when the command is flagged, `None` when
/// it looks fine. False positives are preferable to false negatives — we
/// only warn, we don't block.
pub(crate) fn is_dangerous(cmd: &str) -> Option<&'static str> {
    let c = cmd.trim();
    let lower = c.to_ascii_lowercase();
    // Fork bomb (verbatim or close).
    if c.replace(' ', "").contains(":(){:|:&};:") {
        return Some("looks like a fork bomb");
    }
    // `rm -rf` against root, home, or a parent path.
    if has_rm_rf_dangerous_target(&lower) {
        return Some("rm -rf against a top-level path");
    }
    // mkfs.* — formats a filesystem.
    if lower.split_whitespace().any(|t| t.starts_with("mkfs.") || t == "mkfs") {
        return Some("mkfs formats a filesystem");
    }
    // dd if=… of=/dev/sdX  — disk overwrite.
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return Some("dd writes raw bytes to a device");
    }
    // Pipe to shell from network — typical curl|sh / wget|sh footgun.
    if (lower.contains("curl ") || lower.contains("wget "))
        && (lower.contains("| sh") || lower.contains("|sh") || lower.contains("| bash") || lower.contains("|bash"))
    {
        return Some("piping network content directly to a shell");
    }
    // Redirect to a raw disk device.
    if let Some(idx) = lower.find("> /dev/sd") {
        let after = &lower[idx + 2..];
        // "> /dev/sda", "> /dev/sdb1", …
        if after.split_whitespace().next().is_some_and(|t| t.starts_with("/dev/sd")) {
            return Some("redirecting to a raw block device");
        }
    }
    // chmod 777 -R … on a top-level dir.
    if lower.contains("chmod") && lower.contains("777") && (lower.contains(" /") || lower.contains(" ~")) {
        return Some("recursive chmod 777 on a top-level path");
    }
    None
}

fn has_rm_rf_dangerous_target(lower: &str) -> bool {
    // Match `rm` with -r or -R (anywhere in flag block) and -f, then look at
    // the remaining arguments for a dangerous target. We split on whitespace
    // and tolerate flag clustering like `-rf`, `-fR`, etc.
    let toks: Vec<&str> = lower.split_whitespace().collect();
    let Some(rm_idx) = toks.iter().position(|t| *t == "rm") else { return false };
    let rest = &toks[rm_idx + 1..];
    let mut has_r = false;
    let mut has_f = false;
    let mut targets: Vec<&str> = Vec::new();
    for tok in rest {
        if let Some(flags) = tok.strip_prefix("--") {
            // long options — only recursive matters here.
            if flags == "recursive" {
                has_r = true;
            }
            if flags == "force" {
                has_f = true;
            }
            continue;
        }
        if let Some(flags) = tok.strip_prefix('-') {
            for c in flags.chars() {
                if c == 'r' || c == 'R' {
                    has_r = true;
                } else if c == 'f' {
                    has_f = true;
                }
            }
            continue;
        }
        targets.push(tok);
    }
    if !(has_r && has_f) {
        return false;
    }
    for t in targets {
        if t == "/" || t == "/*" {
            return true;
        }
        if t == "~" || t == "$home" || t.starts_with("~/") {
            return true;
        }
        // Top-level system dirs.
        if matches!(
            t,
            "/bin" | "/boot" | "/etc" | "/home" | "/lib" | "/lib64" | "/opt" | "/root" | "/sbin"
              | "/srv" | "/sys" | "/usr" | "/var" | "/proc" | "/dev"
        ) {
            return true;
        }
        if t.starts_with("/home/") && t.matches('/').count() == 2 {
            // /home/<user> — whole user dir.
            return true;
        }
    }
    false
}

/// Live state for one agent conversation. Held in `AppModel.active_agent`
/// behind an `Rc<RefCell<Option<…>>>` — opening a new session replaces it,
/// closing the dialog clears it.
pub(crate) struct AgentSession {
    pub transcript: Vec<Turn>,
    /// Set when we've sent a command to the active pane and are waiting
    /// for the corresponding `BlockFinished` event. Stores the command
    /// text so we can match it against the finished block (the user may
    /// have typed something else in between).
    pub awaiting_command: Option<String>,
    /// Flag flipped to true on dialog close / max-turns reached. Pending
    /// LLM callbacks check this and bail.
    pub cancelled: Arc<AtomicBool>,
    /// How many model turns we've spent this session (incremented at
    /// each `next_turn` call). Compared against `agent_max_turns`.
    pub turns_used: u32,
    /// UI handles. None when the panel is being built; populated by
    /// `show_agent_panel` after the dialog is constructed so re-render
    /// can find them.
    pub transcript_box: Option<gtk::Box>,
    pub spinner: Option<gtk::Spinner>,
    pub status_label: Option<gtk::Label>,
    pub send_btn: Option<gtk::Button>,
    pub input_entry: Option<gtk::Entry>,
    /// Held so dropping the session cancels an in-flight LLM request.
    pub in_flight: Option<crate::ai::AiHandle>,
    /// Tab + pane the session is bound to. Commands are typed into this
    /// pane only; a BlockFinished from a different pane is ignored even
    /// if the command text matches.
    pub bound_tab: u64,
    pub bound_pane: u64,
    /// `true` once we've reached `agent_max_turns` or the user explicitly
    /// stopped — Send is greyed out, future LLM replies dropped.
    pub sealed: bool,
}

impl AgentSession {
    pub(crate) fn new(bound_tab: u64, bound_pane: u64) -> Self {
        Self {
            transcript: Vec::new(),
            awaiting_command: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            turns_used: 0,
            transcript_box: None,
            spinner: None,
            status_label: None,
            send_btn: None,
            input_entry: None,
            in_flight: None,
            bound_tab,
            bound_pane,
            sealed: false,
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Build the user-side prompt for the next LLM turn. The system prompt
    /// lives in `ai::build_agent_system_prompt` — this is just the
    /// transcript dump.
    pub(crate) fn build_user_prompt(&self) -> String {
        let mut lines: Vec<String> = self.transcript.iter().map(Turn::to_prompt_line).collect();
        // Final hint to nudge JSON output.
        lines.push(
            "Reply with one JSON object per the protocol. Do not wrap in markdown.".to_string(),
        );
        let full = lines.join("\n\n");
        elide_middle(&full, MAX_TRANSCRIPT_BYTES)
    }
}

/// Sample raw command output for the model. Head + tail elision keeps the
/// beginning (where errors usually surface) and the end (where summary
/// lines live) while bounding bytes.
pub(crate) fn sample_observation(output: &str) -> String {
    elide_middle(output, MAX_OBS_BYTES)
}

/// Build and present the agent panel. The dialog is modal-ish (adw::Dialog).
/// All user actions dispatch back through `sender` as variants of
/// `crate::AppMsg` — we don't manipulate AppModel state directly here.
///
/// `session` is the freshly-constructed session whose UI handles we
/// populate on the way out. The caller is expected to store the same
/// `Rc` in `AppModel.active_agent` so update handlers can find it.
pub(crate) fn show_agent_panel(
    window: &adw::ApplicationWindow,
    session: &Rc<RefCell<Option<AgentSession>>>,
    sender: ComponentSender<crate::AppModel>,
    provider_name: &str,
    max_turns: u32,
) {
    let dialog = adw::Dialog::builder()
        .title("AI agent")
        .content_width(820)
        .content_height(640)
        .build();
    let header = adw::HeaderBar::new();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let intro = gtk::Label::new(Some(&format!(
        "Talk to {provider_name}. The model proposes one command per turn; you approve each before it runs. Output is fed back automatically. Max {max_turns} turns."
    )));
    intro.set_wrap(true);
    intro.set_halign(gtk::Align::Start);
    intro.add_css_class("dim-label");
    content.append(&intro);

    let transcript_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    transcript_box.set_margin_top(4);
    let transcript_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&transcript_box)
        .build();
    content.append(&transcript_scroll);

    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let status_label = gtk::Label::new(Some(""));
    status_label.set_halign(gtk::Align::Start);
    status_label.set_hexpand(true);
    status_label.add_css_class("dim-label");
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    status_row.append(&status_label);
    status_row.append(&spinner);
    content.append(&status_row);

    let input_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let input = gtk::Entry::builder()
        .placeholder_text("What do you want to do? (Enter to send)")
        .hexpand(true)
        .build();
    let send_btn = gtk::Button::with_label("Send");
    send_btn.add_css_class("suggested-action");
    input_row.append(&input);
    input_row.append(&send_btn);
    content.append(&input_row);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));

    {
        let mut sess = session.borrow_mut();
        if let Some(sess) = sess.as_mut() {
            sess.transcript_box = Some(transcript_box.clone());
            sess.spinner = Some(spinner.clone());
            sess.status_label = Some(status_label.clone());
            sess.send_btn = Some(send_btn.clone());
            sess.input_entry = Some(input.clone());
        }
    }

    {
        let sender = sender.clone();
        let input = input.clone();
        send_btn.connect_clicked(move |_| {
            let text = input.text().to_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            input.set_text("");
            sender.input(crate::AppMsg::AgentSend(trimmed.to_string()));
        });
    }
    {
        let sender = sender.clone();
        let input_for_activate = input.clone();
        input.connect_activate(move |entry| {
            let text = entry.text().to_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            input_for_activate.set_text("");
            sender.input(crate::AppMsg::AgentSend(trimmed.to_string()));
        });
    }

    {
        let sender = sender.clone();
        dialog.connect_closed(move |_| {
            sender.input(crate::AppMsg::AgentClose);
        });
    }

    dialog.present(Some(window));
    input.grab_focus();
    rerender(session, sender);
}

/// Rebuild the transcript view from the current session state. Cheap to
/// call after every transcript mutation — the panel typically holds a
/// dozen widgets at most.
pub(crate) fn rerender(
    session: &Rc<RefCell<Option<AgentSession>>>,
    sender: ComponentSender<crate::AppModel>,
) {
    let sess_borrow = session.borrow();
    let Some(sess) = sess_borrow.as_ref() else { return };
    let Some(tb) = sess.transcript_box.as_ref() else { return };

    // Clear existing children.
    while let Some(child) = tb.first_child() {
        tb.remove(&child);
    }

    for (idx, turn) in sess.transcript.iter().enumerate() {
        match turn {
            Turn::User(msg) => tb.append(&render_user(msg)),
            Turn::AssistantThought(msg) => tb.append(&render_thought(msg)),
            Turn::AssistantSay(msg) => tb.append(&render_say(msg)),
            Turn::AssistantProposed { cmd, approved } => {
                tb.append(&render_proposed(idx, cmd, *approved, sender.clone(), sess.sealed));
            }
            Turn::Observation { exit, output_sample } => {
                tb.append(&render_observation(*exit, output_sample));
            }
        }
    }

    if let Some(status) = sess.status_label.as_ref() {
        let txt = if sess.sealed {
            format!("Session sealed — open a new agent for more turns.  ({}/{})", sess.turns_used, sess.turns_used.max(1))
        } else if sess.awaiting_command.is_some() {
            "Waiting for command output…".to_string()
        } else {
            format!("turn {}/{}", sess.turns_used, sess.turns_used.max(1))
        };
        status.set_text(&txt);
    }
    if let Some(send) = sess.send_btn.as_ref() {
        send.set_sensitive(!sess.sealed);
    }
    if let Some(input) = sess.input_entry.as_ref() {
        input.set_sensitive(!sess.sealed);
    }
}

fn render_user(msg: &str) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    frame.set_halign(gtk::Align::End);
    let l = gtk::Label::new(Some(msg));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_margin_bottom(8);
    l.set_margin_start(10);
    l.set_margin_end(10);
    l.set_selectable(true);
    frame.set_child(Some(&l));
    frame.upcast()
}

fn render_thought(msg: &str) -> gtk::Widget {
    let l = gtk::Label::new(Some(&format!("💭 {msg}")));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_halign(gtk::Align::Start);
    l.add_css_class("dim-label");
    l.set_selectable(true);
    l.upcast()
}

fn render_say(msg: &str) -> gtk::Widget {
    let l = gtk::Label::new(Some(msg));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_halign(gtk::Align::Start);
    l.set_selectable(true);
    l.upcast()
}

fn render_proposed(
    idx: usize,
    cmd: &str,
    approved: Option<bool>,
    sender: ComponentSender<crate::AppModel>,
    sealed: bool,
) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let danger = is_dangerous(cmd);
    if let Some(reason) = danger {
        let warn = gtk::Label::new(Some(&format!("⚠ destructive — {reason}")));
        warn.add_css_class("error");
        warn.set_halign(gtk::Align::Start);
        outer.append(&warn);
    }

    let cmd_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    cmd_view.buffer().set_text(cmd);
    cmd_view.add_css_class("ai-explain-body");
    outer.append(&cmd_view);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btn_row.set_halign(gtk::Align::End);
    match approved {
        None => {
            let approve = gtk::Button::with_label(if danger.is_some() {
                "Approve & Run (destructive)"
            } else {
                "Approve & Run"
            });
            if danger.is_some() {
                approve.add_css_class("destructive-action");
            } else {
                approve.add_css_class("suggested-action");
            }
            if sealed {
                approve.set_sensitive(false);
            }
            let edit = gtk::Button::with_label("Edit");
            let reject = gtk::Button::with_label("Reject");
            {
                let sender = sender.clone();
                approve.connect_clicked(move |_| sender.input(crate::AppMsg::AgentApprove(idx)));
            }
            {
                let sender = sender.clone();
                let cmd_str = cmd.to_string();
                edit.connect_clicked(move |btn| {
                    let parent = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
                    show_edit_dialog(parent.as_ref(), &cmd_str, sender.clone(), idx);
                });
            }
            {
                let sender = sender.clone();
                reject.connect_clicked(move |_| sender.input(crate::AppMsg::AgentReject(idx)));
            }
            btn_row.append(&reject);
            btn_row.append(&edit);
            btn_row.append(&approve);
        }
        Some(true) => {
            let l = gtk::Label::new(Some("✓ ran"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
        Some(false) => {
            let l = gtk::Label::new(Some("✗ rejected"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
    }
    outer.append(&btn_row);
    frame.set_child(Some(&outer));
    frame.upcast()
}

fn render_observation(exit: i32, output_sample: &str) -> gtk::Widget {
    let exp = gtk::Expander::new(Some(&format!(
        "Output (exit {exit}, {} bytes)",
        output_sample.len()
    )));
    if exit != 0 {
        exp.add_css_class("error");
    }
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    view.buffer().set_text(output_sample);
    view.add_css_class("ai-explain-body");
    let scroll = gtk::ScrolledWindow::builder()
        .height_request(180)
        .child(&view)
        .build();
    exp.set_child(Some(&scroll));
    exp.upcast()
}

fn show_edit_dialog(
    parent: Option<&gtk::Window>,
    initial: &str,
    sender: ComponentSender<crate::AppModel>,
    idx: usize,
) {
    let dialog = adw::Dialog::builder()
        .title("Edit command")
        .content_width(560)
        .content_height(180)
        .build();
    let header = adw::HeaderBar::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let entry = gtk::Entry::builder().text(initial).hexpand(true).build();
    content.append(&entry);
    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btn_row.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let run = gtk::Button::with_label("Run");
    run.add_css_class("suggested-action");
    btn_row.append(&cancel);
    btn_row.append(&run);
    content.append(&btn_row);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));

    let dialog_for_cancel = dialog.clone();
    cancel.connect_clicked(move |_| {
        let _ = dialog_for_cancel.close();
    });

    {
        let dialog = dialog.clone();
        let entry = entry.clone();
        let sender = sender.clone();
        run.connect_clicked(move |_| {
            let new_cmd = entry.text().to_string();
            let trimmed = new_cmd.trim();
            if trimmed.is_empty() {
                return;
            }
            sender.input(crate::AppMsg::AgentEditAndApprove(idx, trimmed.to_string()));
            dialog.close();
        });
    }
    {
        let run = run.clone();
        entry.connect_activate(move |_| {
            run.emit_clicked();
        });
    }

    dialog.present(parent);
}

fn elide_middle(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let half = max_bytes / 2;
    // Find char boundaries to avoid slicing inside a UTF-8 codepoint.
    let mut head_end = half.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(half);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start <= head_end {
        return s[..head_end].to_string();
    }
    let elided = s.len() - (head_end + (s.len() - tail_start));
    format!(
        "{}\n\n… [{} bytes elided] …\n\n{}",
        &s[..head_end],
        elided,
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_run_basic() {
        let r = parse_action(r#"{"action":"run","command":"ls -la"}"#);
        assert_eq!(
            r,
            ParsedAction::Run {
                thought: None,
                command: "ls -la".to_string()
            }
        );
    }

    #[test]
    fn parse_action_run_with_thought() {
        let r = parse_action(
            r#"{"thought":"need to inspect","action":"run","command":"du -sh"}"#,
        );
        match r {
            ParsedAction::Run { thought, command } => {
                assert_eq!(thought.as_deref(), Some("need to inspect"));
                assert_eq!(command, "du -sh");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_action_say() {
        let r = parse_action(r#"{"action":"say","message":"What dir?"}"#);
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "What dir?".to_string()
            }
        );
    }

    #[test]
    fn parse_action_done() {
        let r = parse_action(r#"{"action":"done","message":"All clear."}"#);
        assert_eq!(
            r,
            ParsedAction::Done {
                thought: None,
                message: "All clear.".to_string()
            }
        );
    }

    #[test]
    fn parse_action_strips_json_fence() {
        let raw = "```json\n{\"action\":\"run\",\"command\":\"echo hi\"}\n```";
        match parse_action(raw) {
            ParsedAction::Run { command, .. } => assert_eq!(command, "echo hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_action_falls_back_to_say_on_garbage() {
        let r = parse_action("hello this is not JSON");
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "hello this is not JSON".to_string()
            }
        );
    }

    #[test]
    fn parse_action_unknown_action_treated_as_say() {
        let r = parse_action(r#"{"action":"frobnicate","message":"huh"}"#);
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "huh".to_string()
            }
        );
    }

    #[test]
    fn parse_action_empty_command_falls_back_to_say() {
        let r = parse_action(r#"{"action":"run","command":""}"#);
        match r {
            ParsedAction::Say { .. } => {}
            other => panic!("expected say, got {other:?}"),
        }
    }

    #[test]
    fn dangerous_catches_rm_rf_root() {
        assert!(is_dangerous("rm -rf /").is_some());
        assert!(is_dangerous("rm -rf /*").is_some());
        assert!(is_dangerous("rm -fr /").is_some());
        assert!(is_dangerous("rm -r -f /").is_some());
        assert!(is_dangerous("rm --recursive --force /").is_some());
    }

    #[test]
    fn dangerous_catches_rm_rf_home() {
        assert!(is_dangerous("rm -rf ~").is_some());
        assert!(is_dangerous("rm -rf ~/").is_some());
        assert!(is_dangerous("rm -rf /home/alice").is_some());
        assert!(is_dangerous("rm -rf /home").is_some());
    }

    #[test]
    fn dangerous_allows_rm_rf_in_tmp() {
        // We deliberately do not flag /tmp/foo — that's the user's call.
        assert!(is_dangerous("rm -rf /tmp/foo").is_none());
        assert!(is_dangerous("rm -rf ./build").is_none());
        assert!(is_dangerous("rm somefile").is_none());
    }

    #[test]
    fn dangerous_catches_mkfs() {
        assert!(is_dangerous("mkfs.ext4 /dev/sda1").is_some());
        assert!(is_dangerous("sudo mkfs.xfs /dev/sdb").is_some());
    }

    #[test]
    fn dangerous_catches_dd_to_device() {
        assert!(is_dangerous("dd if=foo of=/dev/sda bs=1M").is_some());
    }

    #[test]
    fn dangerous_catches_curl_pipe_sh() {
        assert!(is_dangerous("curl https://foo.sh | sh").is_some());
        assert!(is_dangerous("wget -qO- https://foo.sh | bash").is_some());
    }

    #[test]
    fn dangerous_catches_fork_bomb() {
        assert!(is_dangerous(":(){ :|:& };:").is_some());
    }

    #[test]
    fn dangerous_lets_normal_commands_through() {
        assert!(is_dangerous("ls -la").is_none());
        assert!(is_dangerous("git status").is_none());
        assert!(is_dangerous("docker ps").is_none());
        assert!(is_dangerous("cargo build --release").is_none());
    }

    #[test]
    fn transcript_prompt_includes_turns() {
        let mut s = AgentSession::new(0, 0);
        s.transcript.push(Turn::User("disk full".to_string()));
        s.transcript.push(Turn::AssistantProposed {
            cmd: "df -h".to_string(),
            approved: Some(true),
        });
        s.transcript.push(Turn::Observation {
            exit: 0,
            output_sample: "Filesystem      Size  Used".to_string(),
        });
        let prompt = s.build_user_prompt();
        assert!(prompt.contains("disk full"));
        assert!(prompt.contains("df -h"));
        assert!(prompt.contains("Filesystem"));
        assert!(prompt.contains("exit=0"));
    }

    #[test]
    fn elide_middle_passes_short_through() {
        assert_eq!(elide_middle("hi", 100), "hi");
    }

    #[test]
    fn elide_middle_truncates_long_text_keeping_head_and_tail() {
        let big = "x".repeat(10_000);
        let s = elide_middle(&big, 1000);
        assert!(s.contains("elided"));
        assert!(s.len() < 1500);
    }

    #[test]
    fn elide_middle_respects_utf8_boundaries() {
        // Multibyte chars near the cut points.
        let s: String = (0..2000).map(|_| "λ").collect();
        let out = elide_middle(&s, 200);
        // Must be valid UTF-8 and contain the elision marker.
        assert!(out.contains("elided"));
        assert!(out.chars().count() > 0);
    }
}
