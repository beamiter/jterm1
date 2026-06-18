//! Block-view terminal backend as a relm4 Component (Warp-style).
//!
//! Drives its own `OwnedPty` (not vte4's) so the raw stream can be intercepted
//! by `crate::parser::Parser` for OSC 133 block detection. Live output is shown
//! in ONE persistent input-disabled `vte4::Terminal` (the "active" card); when a
//! command finishes, its command + output are snapshotted into a plain
//! `gtk4::TextView` "finished block" stacked above the active card. Input is
//! captured from the active VTE's `commit` signal and forwarded to the PTY.
//!
//! Mirrors `VteTerminal`'s Component surface — same `VteInit`/`VteInput`/
//! `VteOutput` types — so `main.rs` can route to either backend by config.

use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::Orientation;
use relm4::gtk;
use relm4::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vte4::{TerminalExt, TerminalExtManual};

use super::ansi::{self, AnsiTextRun};
use super::url;
use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;

pub use super::vte::{VteInit, VteInput, VteOutput};

// ─── Block state machine ────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockState {
    /// No OSC 133 marks seen yet — waiting to learn if the shell has integration.
    Idle,
    /// Shell has no OSC 133 integration: stream everything to the active VTE raw.
    RawFallback,
    /// Between OSC 133 ;A and ;B — prompt is rendering.
    CollectingPrompt,
    /// Between ;B and ;C — user is typing the command.
    AwaitingCommand,
    /// Between ;C and ;D — command output is streaming.
    CollectingOutput,
    /// After ;D — command finished, finalize deferred to next ;A.
    PostCommand,
    /// Inside alt-screen app (vim/less/htop).
    AltScreen,
}

/// Which finished blocks are shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockFilter {
    None,
    Failed,
    Slow,
    Pinned,
}

/// Metadata + widget for one finished block, used by filtering,
/// the right-click context menu, and export.
struct FinishedBlock {
    /// Stable identity (monotonic), so context-menu closures can find this block
    /// after deletions have shifted vector positions.
    id: u64,
    widget: gtk::Box,
    /// The command TextView, kept for in-block search highlighting.
    command_view: gtk::TextView,
    /// The output TextView (None when the command produced no output), kept for
    /// search highlight, error-jump, and lazy/collapse re-rendering.
    output_view: Option<gtk::TextView>,
    /// The "show N more lines" button, when output was truncated.
    show_more: Option<gtk::Button>,
    /// Full styled output runs, cached so the output view can be re-truncated
    /// (reversible collapse) or rendered on demand (lazy load).
    full_runs: Rc<Vec<AnsiTextRun>>,
    /// Whether the output area is currently collapsed (hidden).
    collapsed: Rc<Cell<bool>>,
    /// Whether the truncated head (vs. full output) is currently shown.
    truncated: Rc<Cell<bool>>,
    /// User-pinned (bookmarked): stays visible under any filter, visually marked.
    pinned: bool,
    /// Header pin glyph, toggled with `pinned`.
    pin_icon: gtk::Label,
    /// Header index badge (1-9), shown only in block-selection mode.
    index_badge: gtk::Label,
    /// Header collapse toggle, kept for collapse-all / toggle-selected.
    collapse_btn: gtk::Button,
    /// Relative-time label ("2m ago"), refreshed by a periodic timer.
    time_label: Option<gtk::Label>,
    /// Buffer char offsets of detected error lines in the output, for n/N jump.
    error_offsets: Vec<i32>,
    /// Cursor into `error_offsets` for n/N cycling.
    error_idx: Cell<usize>,
    /// Last line of the captured shell prompt (best-effort), for Copy/export.
    prompt: String,
    command: String,
    /// De-styled output text, kept for full-text search, copy, and export.
    plain_output: String,
    exit_code: i32,
    cwd: String,
    duration_ms: u64,
    /// Wall-clock command-end time (ms since epoch), for export parity.
    end_time_ms: Option<u64>,
}

/// A command slower than this (ms) counts as "slow" for the slow filter.
const SLOW_THRESHOLD_MS: u64 = 1000;

/// A command taking at least this long (ms) triggers a completion notification
/// (tab highlight) when the user isn't looking at this terminal.
const NOTIFY_THRESHOLD_MS: u64 = 3000;

/// Keyboard-nav legend shown in the bottom hint bar while a block is selected.
const HINT_TEXT: &str =
    "j/k move · Enter recall · n/N errors · 1-9 jump · y copy · Space fold · ,/. fold all · ? help · Esc exit";

// ─── Shared reader/handler context ──────────────────────────────────────────

/// State touched by both the PTY reader (on the GLib main thread) and the
/// component `update`. All single-threaded; `Rc`/`Cell`/`RefCell` suffice.
struct Ctx {
    config: Rc<RefCell<Config>>,
    pty: Rc<OwnedPty>,
    active_vte: vte4::Terminal,
    block_list: gtk::Box,
    active_holder: gtk::Box,
    scroll: gtk::ScrolledWindow,
    parser: RefCell<Parser>,
    state: Cell<BlockState>,
    prev_state: Cell<BlockState>,
    cmd_buf: RefCell<Vec<u8>>,
    /// Command text reconstructed from the active VTE's `commit` keystrokes
    /// (cleaner than scraping the autosuggestion-redrawn output stream).
    typed_cmd: RefCell<String>,
    /// Raw prompt bytes buffered between PromptStart and PromptEnd.
    prompt_buf: RefCell<Vec<u8>>,
    /// Last captured prompt (de-styled, last line), kept for Copy/export.
    prompt: RefCell<String>,
    out_buf: RefCell<Vec<u8>>,
    exit_code: Cell<i32>,
    cwd: RefCell<String>,
    start_time: Cell<Option<Instant>>,
    duration: Cell<Option<Duration>>,
    /// Wall-clock time the last command finished (ms since epoch).
    end_time_ms: Cell<Option<u64>>,
    has_command: Cell<bool>,
    /// Monotonic id source for finished blocks (stable across deletions).
    next_block_id: Cell<u64>,
    /// Finished blocks in display order (top→bottom), for filtering.
    finished: RefCell<Vec<FinishedBlock>>,
    filter: Cell<BlockFilter>,
    /// Indices into `finished` matching the current search query, plus a cursor
    /// into that list for next/prev cycling.
    search_matches: RefCell<Vec<usize>>,
    search_idx: Cell<usize>,
    /// Index into `finished` of the keyboard-selected block (Warp-style block
    /// recall), or `None` when nothing is selected.
    selected_block: Cell<Option<usize>>,
    /// Pager frames captured while inside the alt-screen, merged on exit so the
    /// finished block keeps `less`/`man`/`git log` content instead of vanishing.
    pager_snapshots: Rc<RefCell<Vec<String>>>,
    /// Bumped on each alt-screen entry to cancel snapshots scheduled before it.
    pager_generation: Rc<Cell<u64>>,
    /// The last frame of the *previous* command, used to drop the stale render
    /// that lingers before the alt VTE's reset paints the new screen.
    pager_preclear: Rc<RefCell<String>>,
    /// True while an alt-screen app owns the viewport (finished blocks hidden).
    fullscreen: Cell<bool>,
    /// Sticky command header floating over the viewport top.
    sticky_header: gtk::Box,
    sticky_label: gtk::Label,
    /// Index into `finished` the sticky header currently points at (for click).
    sticky_idx: Cell<Option<usize>>,
    /// Bottom hint bar (keyboard-nav legend) shown while a block is selected.
    hint_bar: gtk::Box,
    /// Bottom-right container for transient completion toasts.
    toast_box: gtk::Box,
    /// Set at CommandEnd when a slow command finished while the user wasn't
    /// watching; consumed in finalize_block to raise a click-to-jump toast.
    pending_toast: Cell<bool>,
    /// Right-edge minimap: one colored tick per visible finished block.
    minimap: gtk::Box,
    /// Generation guard so a newer animated scroll cancels the previous one.
    scroll_anim_gen: Cell<u64>,
    /// Whether the view is "stuck" to the bottom: when true, content/height
    /// changes re-pin the scroll to the bottom so the active input cell stays
    /// visible. Cleared when the user scrolls up, restored when they return.
    stick_bottom: Cell<bool>,
    /// Live substring filter over block commands (AND-ed with the preset filter).
    filter_query: RefCell<String>,
    /// Revealer wrapping the live-filter entry at the top of the view.
    filter_revealer: gtk::Revealer,
    /// The live-filter text entry.
    filter_entry: gtk::SearchEntry,
}

// ─── Component ──────────────────────────────────────────────────────────────

pub struct BlockTerminal {
    ctx: Rc<Ctx>,
    config: Rc<RefCell<Config>>,
}

impl Component for BlockTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk::Box;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Box::new(Orientation::Vertical, 0)
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        install_block_css(&init.config.borrow());

        root.set_hexpand(true);
        root.set_vexpand(true);

        // Scroll → viewport → block_list (vertical stack of blocks).
        let block_list = gtk::Box::new(Orientation::Vertical, 6);
        block_list.add_css_class("block-list");
        block_list.set_hexpand(true);
        block_list.set_vexpand(true);

        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&block_list)
            .build();
        scroll.add_css_class("block-scroll");

        // Sticky command header: floats at the top of the viewport, showing the
        // command of the finished block currently scrolled under the top edge.
        let overlay = gtk::Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        overlay.set_child(Some(&scroll));

        let sticky_header = gtk::Box::new(Orientation::Horizontal, 0);
        sticky_header.add_css_class("block-sticky-header");
        sticky_header.set_halign(gtk::Align::Fill);
        sticky_header.set_valign(gtk::Align::Start);
        sticky_header.set_visible(false);
        let sticky_label = gtk::Label::new(None);
        sticky_label.set_halign(gtk::Align::Start);
        sticky_label.set_hexpand(true);
        sticky_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sticky_label.add_css_class("block-sticky-label");
        sticky_header.append(&sticky_label);
        overlay.add_overlay(&sticky_header);

        // Bottom hint bar: the keyboard-nav legend, revealed while a block is
        // selected so the (otherwise invisible) vim-style nav is discoverable.
        let hint_bar = gtk::Box::new(Orientation::Horizontal, 0);
        hint_bar.add_css_class("block-hint-bar");
        hint_bar.set_halign(gtk::Align::Center);
        hint_bar.set_valign(gtk::Align::End);
        hint_bar.set_visible(false);
        hint_bar.set_can_target(false);
        let hint_label = gtk::Label::new(Some(HINT_TEXT));
        hint_label.add_css_class("block-hint-label");
        hint_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        hint_bar.append(&hint_label);
        overlay.add_overlay(&hint_bar);

        // Bottom-right toast stack for off-screen completion notices.
        let toast_box = gtk::Box::new(Orientation::Vertical, 6);
        toast_box.add_css_class("block-toast-box");
        toast_box.set_halign(gtk::Align::End);
        toast_box.set_valign(gtk::Align::End);
        overlay.add_overlay(&toast_box);

        // Right-edge minimap: a strip of colored ticks, one per visible block.
        let minimap = gtk::Box::new(Orientation::Vertical, 2);
        minimap.add_css_class("block-minimap");
        minimap.set_halign(gtk::Align::End);
        minimap.set_valign(gtk::Align::Fill);
        minimap.set_homogeneous(true);
        minimap.set_visible(false);
        overlay.add_overlay(&minimap);

        // Live command-filter entry, slid in from the top on demand.
        let filter_entry = gtk::SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Filter blocks by command…"));
        filter_entry.add_css_class("block-filter-entry");
        let filter_revealer = gtk::Revealer::new();
        filter_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        filter_revealer.set_child(Some(&filter_entry));
        filter_revealer.set_reveal_child(false);
        root.append(&filter_revealer);
        root.append(&overlay);

        // The persistent active card. `input_enabled` must stay true so VTE emits
        // the `commit` signal we forward to our PTY; it has no child PTY of its
        // own, so VTE's own write goes nowhere — only our forward matters.
        let active_vte = super::vte::create_terminal(&init.config.borrow());
        active_vte.set_vexpand(true);
        active_vte.set_hexpand(true);

        let active_holder = gtk::Box::new(Orientation::Vertical, 0);
        active_holder.add_css_class("block-active");
        active_holder.set_hexpand(true);
        active_holder.set_vexpand(true);
        active_holder.append(&active_vte);
        block_list.append(&active_holder);

        // Spawn the shell on a fresh PTY.
        let argv: Vec<&str> = init.shell_argv.iter().map(|s| s.as_str()).collect();
        let home = std::env::var("HOME").ok();
        let cwd = init.working_directory.clone().or(home);
        let pty = OwnedPty::spawn(&argv, cwd.as_deref(), &[])
            .expect("failed to spawn block-view PTY");
        let pty = Rc::new(pty);
        init.probe.shell_pid.set(pty.pid_i32());
        init.probe.pty_fd.set(pty.master_fd_raw());

        let ctx = Rc::new(Ctx {
            config: init.config.clone(),
            pty: pty.clone(),
            active_vte: active_vte.clone(),
            block_list: block_list.clone(),
            active_holder: active_holder.clone(),
            scroll: scroll.clone(),
            parser: RefCell::new(Parser::new()),
            state: Cell::new(BlockState::Idle),
            prev_state: Cell::new(BlockState::Idle),
            cmd_buf: RefCell::new(Vec::new()),
            typed_cmd: RefCell::new(String::new()),
            prompt_buf: RefCell::new(Vec::new()),
            prompt: RefCell::new(String::new()),
            out_buf: RefCell::new(Vec::new()),
            exit_code: Cell::new(0),
            cwd: RefCell::new(init.working_directory.clone().unwrap_or_default()),
            start_time: Cell::new(None),
            duration: Cell::new(None),
            end_time_ms: Cell::new(None),
            has_command: Cell::new(false),
            next_block_id: Cell::new(0),
            finished: RefCell::new(Vec::new()),
            filter: Cell::new(BlockFilter::None),
            search_matches: RefCell::new(Vec::new()),
            search_idx: Cell::new(0),
            selected_block: Cell::new(None),
            pager_snapshots: Rc::new(RefCell::new(Vec::new())),
            pager_generation: Rc::new(Cell::new(0)),
            pager_preclear: Rc::new(RefCell::new(String::new())),
            fullscreen: Cell::new(false),
            sticky_header: sticky_header.clone(),
            sticky_label: sticky_label.clone(),
            sticky_idx: Cell::new(None),
            hint_bar: hint_bar.clone(),
            toast_box: toast_box.clone(),
            pending_toast: Cell::new(false),
            minimap: minimap.clone(),
            scroll_anim_gen: Cell::new(0),
            stick_bottom: Cell::new(true),
            filter_query: RefCell::new(String::new()),
            filter_revealer: filter_revealer.clone(),
            filter_entry: filter_entry.clone(),
        });

        // Size the active card to its content (warp-style): compact while typing,
        // growing as a command streams output, full-screen only for alt-screen
        // apps / no-OSC133 shells. Re-clamp whenever the viewport changes (resize).
        // `changed` fires after layout with a fresh `upper`, so this is also where
        // we re-pin to the bottom (keeping the active input cell visible) once the
        // grown/shrunk content is laid out — the immediate scroll in `autoscroll`
        // runs against a stale `upper` and would otherwise land short.
        {
            let ctx = ctx.clone();
            scroll.vadjustment().connect_changed(move |adj| {
                update_active_height(&ctx);
                if ctx.stick_bottom.get() && !ctx.fullscreen.get() {
                    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
                }
            });
        }

        // Sticky command header + stick-to-bottom tracking: as the user scrolls,
        // show the command under the top edge, and remember whether they are at the
        // bottom so output/height changes only auto-follow when they want them to.
        {
            let ctx = ctx.clone();
            scroll.vadjustment().connect_value_changed(move |adj| {
                let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                ctx.stick_bottom.set(adj.value() >= max_val - 4.0);
                update_sticky_header(&ctx);
            });
        }
        // Click the sticky header to jump back to the top of that block.
        {
            let ctx = ctx.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_g, _n, _x, _y| {
                if let Some(idx) = ctx.sticky_idx.get() {
                    scroll_to_block(&ctx, idx);
                }
            });
            sticky_header.add_controller(click);
        }

        // Forward keystrokes from the active VTE to our PTY, and reconstruct the
        // typed command line while we are between prompt-end and command-start.
        {
            let pty = pty.clone();
            let ctx = ctx.clone();
            active_vte.connect_commit(move |_term, text, _size| {
                pty.write_bytes(text.as_bytes());
                if ctx.state.get() == BlockState::AwaitingCommand {
                    let mut typed = ctx.typed_cmd.borrow_mut();
                    for ch in text.chars() {
                        match ch {
                            '\r' | '\n' => {}
                            '\u{7f}' | '\u{8}' => {
                                typed.pop();
                            }
                            c if (c as u32) < 0x20 => {}
                            c => typed.push(c),
                        }
                    }
                }
            });
        }

        // Track size changes and resize the PTY accordingly. Also re-pin the
        // scroll to the bottom each frame while stuck there: the active cell's
        // height_request resize lands a frame or two after the data arrives, and
        // the vadjustment `changed` signal can fire against a stale `upper`, so a
        // per-frame pin is what reliably keeps the active input cell visible
        // (without it the view only settled after an unrelated relayout, e.g. a
        // mouse move).
        {
            let pty = pty.clone();
            let ctx = ctx.clone();
            let last = Rc::new(Cell::new((0i64, 0i64)));
            active_vte.add_tick_callback(move |term, _clock| {
                let cols = term.column_count();
                let rows = term.row_count();
                if (cols, rows) != last.get() && cols > 0 && rows > 0 {
                    last.set((cols, rows));
                    pty.resize(cols as u16, rows as u16);
                }
                if ctx.stick_bottom.get() && !ctx.fullscreen.get() {
                    let adj = ctx.scroll.vadjustment();
                    let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                    if (adj.value() - max_val).abs() > 0.5 {
                        adj.set_value(max_val);
                    }
                }
                glib::ControlFlow::Continue
            });
        }

        // Restore previously-persisted finished blocks (if history is configured).
        load_block_history(&ctx);
        rebuild_minimap(&ctx);

        // Live command filter: typing narrows the block list by command substring.
        {
            let ctx = ctx.clone();
            filter_entry.connect_search_changed(move |e| {
                *ctx.filter_query.borrow_mut() = e.text().to_string();
                apply_visibility(&ctx);
            });
        }
        {
            let ctx = ctx.clone();
            filter_entry.connect_stop_search(move |_| {
                ctx.filter_revealer.set_reveal_child(false);
                ctx.filter_entry.set_text("");
                ctx.filter_query.borrow_mut().clear();
                apply_visibility(&ctx);
                ctx.active_vte.grab_focus();
            });
        }

        // Periodically refresh the relative-time labels ("2m ago").
        {
            let ctx = ctx.clone();
            glib::timeout_add_seconds_local(30, move || {
                refresh_relative_times(&ctx);
                glib::ControlFlow::Continue
            });
        }

        // Install the reader: parser events drive the block state machine.
        {
            let ctx = ctx.clone();
            let sender = sender.clone();
            let on_exit_sender = sender.clone();
            pty.start_reader(
                move |data| handle_data(&ctx, &sender, &data),
                move |code| {
                    let _ = on_exit_sender.output(VteOutput::Exited(code));
                },
            );
        }

        // Feed startup commands once the shell is ready.
        if let Some(cmds) = init.initial_commands.clone() {
            if !cmds.is_empty() {
                let pty = pty.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    for line in cmds.split(", ") {
                        let text = format!("{}\r", line.trim());
                        pty.write_bytes(text.as_bytes());
                    }
                });
            }
        }

        {
            let av = active_vte.clone();
            active_vte.connect_realize(move |_| {
                av.grab_focus();
            });
        }

        {
            let sender = sender.clone();
            let focus_ctl = gtk::EventControllerFocus::new();
            focus_ctl.connect_enter(move |_| {
                let _ = sender.output(VteOutput::Focused);
            });
            active_vte.add_controller(focus_ctl);
        }

        // Block-local navigation (Warp-style). Capture phase so these fire before
        // the VTE's own key handling; all combos are unbound globally, so the
        // window-root key controller passes them through to here.
        {
            let ctx = ctx.clone();
            // Tracks the time of the last bare `g` press for `gg` chord detection.
            let last_g: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
            let key_ctl = gtk::EventControllerKey::new();
            key_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_ctl.connect_key_pressed(move |_c, keyval, _kc, state| {
                use gtk::gdk::Key;
                use gtk::gdk::ModifierType as Mod;
                let ctrl = state.contains(Mod::CONTROL_MASK);
                let shift = state.contains(Mod::SHIFT_MASK);
                let alt = state.contains(Mod::ALT_MASK);

                // Shift+PageUp/PageDown: page the block list locally.
                if shift && !ctrl && !alt && matches!(keyval, Key::Page_Up | Key::Page_Down) {
                    let adj = ctx.scroll.vadjustment();
                    let step = (adj.page_size() * 0.9).max(1.0);
                    let delta = if keyval == Key::Page_Up { -step } else { step };
                    let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                    adj.set_value((adj.value() + delta).clamp(adj.lower(), max_val));
                    return glib::Propagation::Stop;
                }

                // Shift+Up/Down: move the finished-block selection.
                if shift && !ctrl && !alt && matches!(keyval, Key::Up | Key::Down) {
                    step_block_selection(&ctx, if keyval == Key::Up { -1 } else { 1 });
                    return glib::Propagation::Stop;
                }

                // Selection-mode single-key navigation (vim-style). Only active
                // while a block is already selected, so normal shell typing is
                // never intercepted.
                if ctx.selected_block.get().is_some() && !ctrl && !alt {
                    match keyval {
                        Key::j | Key::Down => {
                            step_block_selection(&ctx, 1);
                            return glib::Propagation::Stop;
                        }
                        Key::k | Key::Up => {
                            step_block_selection(&ctx, -1);
                            return glib::Propagation::Stop;
                        }
                        Key::Home => {
                            jump_block_edge(&ctx, true);
                            return glib::Propagation::Stop;
                        }
                        Key::End | Key::G => {
                            jump_block_edge(&ctx, false);
                            return glib::Propagation::Stop;
                        }
                        Key::g => {
                            let now = Instant::now();
                            let dbl = last_g
                                .get()
                                .map(|t| now.duration_since(t) < Duration::from_millis(500))
                                .unwrap_or(false);
                            if dbl {
                                jump_block_edge(&ctx, true);
                                last_g.set(None);
                            } else {
                                last_g.set(Some(now));
                            }
                            return glib::Propagation::Stop;
                        }
                        Key::n => {
                            jump_to_error(&ctx, 1);
                            return glib::Propagation::Stop;
                        }
                        Key::N => {
                            jump_to_error(&ctx, -1);
                            return glib::Propagation::Stop;
                        }
                        Key::y | Key::Y => {
                            if let Some(i) = ctx.selected_block.get() {
                                let id = ctx.finished.borrow().get(i).map(|b| b.id);
                                if let Some(id) = id {
                                    copy_block_by_id(&ctx, id);
                                }
                            }
                            return glib::Propagation::Stop;
                        }
                        Key::space => {
                            toggle_selected_collapse(&ctx);
                            return glib::Propagation::Stop;
                        }
                        Key::comma => {
                            set_all_collapsed(&ctx, true);
                            return glib::Propagation::Stop;
                        }
                        Key::period => {
                            set_all_collapsed(&ctx, false);
                            return glib::Propagation::Stop;
                        }
                        Key::slash => {
                            ctx.filter_revealer.set_reveal_child(true);
                            ctx.filter_entry.grab_focus();
                            return glib::Propagation::Stop;
                        }
                        Key::question => {
                            show_cheatsheet(&ctx);
                            return glib::Propagation::Stop;
                        }
                        _ => {
                            if let Some(c) = keyval.to_unicode() {
                                if ('1'..='9').contains(&c) {
                                    jump_to_nth_visible(&ctx, c as usize - '1' as usize);
                                    return glib::Propagation::Stop;
                                }
                            }
                        }
                    }
                }

                // Enter while a block is selected: recall its command into the
                // input line (clear the shell line with Ctrl+U, then type it).
                if matches!(keyval, Key::Return | Key::KP_Enter) {
                    if let Some(idx) = ctx.selected_block.get() {
                        let cmd = ctx
                            .finished
                            .borrow()
                            .get(idx)
                            .map(|b| b.command.clone());
                        if let Some(cmd) = cmd {
                            ctx.pty.write_bytes(b"\x15");
                            ctx.pty.write_bytes(cmd.as_bytes());
                            ctx.typed_cmd.borrow_mut().clear();
                        }
                        select_block(&ctx, None);
                        return glib::Propagation::Stop;
                    }
                    return glib::Propagation::Proceed;
                }

                // Escape clears the block selection (when one is active).
                if keyval == Key::Escape && ctx.selected_block.get().is_some() {
                    select_block(&ctx, None);
                    return glib::Propagation::Stop;
                }

                // Escape on an empty prompt enters block-nav mode by selecting
                // the last visible block (the discoverable entry point). Guarded
                // on an empty typed line so it never hijacks shell editing / vi-mode.
                if keyval == Key::Escape
                    && !ctrl
                    && !alt
                    && ctx.selected_block.get().is_none()
                    && ctx.typed_cmd.borrow().is_empty()
                    && matches!(
                        ctx.state.get(),
                        BlockState::AwaitingCommand | BlockState::RawFallback | BlockState::Idle
                    )
                {
                    let visible = visible_indices(&ctx);
                    if let Some(&idx) = visible.last() {
                        select_block(&ctx, Some(idx));
                        return glib::Propagation::Stop;
                    }
                }

                // Ctrl+L: clear visible finished blocks + send form feed.
                if ctrl && !shift && !alt && matches!(keyval, Key::l | Key::L) {
                    clear_visible_blocks(&ctx);
                    return glib::Propagation::Stop;
                }

                glib::Propagation::Proceed
            });
            active_vte.add_controller(key_ctl);
        }

        let model = BlockTerminal {
            ctx,
            config: init.config,
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            VteInput::WriteInput(data) => self.ctx.pty.write_bytes(&data),
            VteInput::Resize(cols, rows) => self.ctx.pty.resize(cols, rows),
            VteInput::GrabFocus => {
                self.ctx.active_vte.grab_focus();
            }
            VteInput::Copy => self
                .ctx
                .active_vte
                .copy_clipboard_format(vte4::Format::Text),
            VteInput::Paste => self.ctx.active_vte.paste_clipboard(),
            VteInput::SetFontScale(scale) => self.ctx.active_vte.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let fd = FontDescription::from_string(&desc);
                self.ctx.active_vte.set_font(Some(&fd));
            }
            VteInput::SetScrollback(lines) => self.ctx.active_vte.set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => {
                let adj = self.ctx.scroll.vadjustment();
                let delta = adj.step_increment() * lines as f64;
                let max_val = adj.upper() - adj.page_size();
                let new_val = (adj.value() + delta).clamp(adj.lower(), max_val.max(adj.lower()));
                adj.set_value(new_val);
            }
            VteInput::ApplyTheme => {
                let config = self.config.borrow();
                let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
                self.ctx.active_vte.set_colors(
                    Some(&config.foreground),
                    Some(&config.background),
                    &palette_refs,
                );
                self.ctx.active_vte.set_color_cursor(Some(&config.cursor));
                self.ctx
                    .active_vte
                    .set_color_cursor_foreground(Some(&config.cursor_foreground));
                drop(config);
                install_block_css(&self.config.borrow());
            }
            VteInput::Kill => self.ctx.pty.kill(),
            VteInput::FilterFailedBlocks => apply_filter(&self.ctx, BlockFilter::Failed),
            VteInput::FilterSlowBlocks => apply_filter(&self.ctx, BlockFilter::Slow),
            VteInput::FilterPinnedBlocks => apply_filter(&self.ctx, BlockFilter::Pinned),
            VteInput::ClearBlockFilter => apply_filter(&self.ctx, BlockFilter::None),
            VteInput::SearchSet(query, use_regex) => search_set(&self.ctx, &query, use_regex),
            VteInput::SearchNext => search_step(&self.ctx, 1),
            VteInput::SearchPrev => search_step(&self.ctx, -1),
            VteInput::SearchClear => {
                clear_search_highlights(&self.ctx);
                self.ctx.search_matches.borrow_mut().clear();
                self.ctx.search_idx.set(0);
            }
        }
    }
}

/// Compute the set of finished blocks matching `query`, highlight the matches in
/// each block's command/output views, and jump to the first.
fn search_set(ctx: &Rc<Ctx>, query: &str, use_regex: bool) {
    clear_search_highlights(ctx);
    if query.is_empty() {
        ctx.search_matches.borrow_mut().clear();
        ctx.search_idx.set(0);
        return;
    }
    let re = if use_regex {
        regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .ok()
    } else {
        None
    };
    let accent = ctx.config.borrow().palette[3];
    let bg = format!(
        "rgba({},{},{},0.40)",
        (accent.red() * 255.0) as u8,
        (accent.green() * 255.0) as u8,
        (accent.blue() * 255.0) as u8,
    );
    let needle = query.to_lowercase();
    let mut matches = Vec::new();
    for (idx, block) in ctx.finished.borrow().iter().enumerate() {
        let hay = format!("{}\n{}", block.command, block.plain_output);
        let hit = match &re {
            Some(re) => re.is_match(&hay),
            None => hay.to_lowercase().contains(&needle),
        };
        if hit {
            matches.push(idx);
            highlight_matches_in_view(&block.command_view, re.as_ref(), &needle, &bg);
            if let Some(view) = &block.output_view {
                highlight_matches_in_view(view, re.as_ref(), &needle, &bg);
            }
        }
    }
    *ctx.search_matches.borrow_mut() = matches;
    ctx.search_idx.set(0);
    if let Some(&first) = ctx.search_matches.borrow().first() {
        scroll_to_block(ctx, first);
    }
}

/// Remove the search-highlight tag from every block's command/output buffers.
fn clear_search_highlights(ctx: &Rc<Ctx>) {
    for block in ctx.finished.borrow().iter() {
        remove_tag(&block.command_view, "jterm-search");
        if let Some(view) = &block.output_view {
            remove_tag(view, "jterm-search");
        }
    }
}

fn remove_tag(view: &gtk::TextView, tag_name: &str) {
    let buffer = view.buffer();
    if buffer.tag_table().lookup(tag_name).is_some() {
        let (s, e) = (buffer.start_iter(), buffer.end_iter());
        buffer.remove_tag_by_name(tag_name, &s, &e);
    }
}

/// Highlight every occurrence of the query within a view's *current* buffer text
/// (so truncated output highlights only what's shown). ASCII case-insensitive for
/// literal queries; regex find for regex queries.
fn highlight_matches_in_view(
    view: &gtk::TextView,
    re: Option<&regex::Regex>,
    needle_lower: &str,
    bg: &str,
) {
    let buffer = view.buffer();
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    let mut ranges: Vec<(i32, i32)> = Vec::new();
    match re {
        Some(re) => {
            for m in re.find_iter(&text) {
                let s = text[..m.start()].chars().count() as i32;
                let e = text[..m.end()].chars().count() as i32;
                if e > s {
                    ranges.push((s, e));
                }
            }
        }
        None => {
            if needle_lower.is_empty() {
                return;
            }
            let hay: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
            let pat: Vec<char> = needle_lower.chars().collect();
            let mut i = 0;
            while i + pat.len() <= hay.len() {
                if hay[i..i + pat.len()] == pat[..] {
                    ranges.push((i as i32, (i + pat.len()) as i32));
                    i += pat.len();
                } else {
                    i += 1;
                }
            }
        }
    }
    if ranges.is_empty() {
        return;
    }
    let table = buffer.tag_table();
    let tag = table.lookup("jterm-search").unwrap_or_else(|| {
        let t = gtk::TextTag::builder().name("jterm-search").background(bg).build();
        table.add(&t);
        t
    });
    for (s, e) in ranges {
        let si = buffer.iter_at_offset(s);
        let ei = buffer.iter_at_offset(e);
        buffer.apply_tag(&tag, &si, &ei);
    }
}

/// Advance the search cursor by `delta` (wrapping) and scroll to that match.
fn search_step(ctx: &Rc<Ctx>, delta: i32) {
    let n = ctx.search_matches.borrow().len() as i32;
    if n == 0 {
        return;
    }
    let cur = ctx.search_idx.get() as i32;
    let next = ((cur + delta) % n + n) % n;
    ctx.search_idx.set(next as usize);
    let target = ctx.search_matches.borrow()[next as usize];
    scroll_to_block(ctx, target);
}

/// Scroll the block list so finished block `idx` is at the viewport top.
/// True when the user is plausibly looking at this terminal: its toplevel
/// window is active AND the view is scrolled near the bottom (the live output).
fn user_is_watching(ctx: &Rc<Ctx>) -> bool {
    let window_active = ctx
        .scroll
        .root()
        .and_downcast::<gtk::Window>()
        .map(|w| w.is_active())
        .unwrap_or(false);
    if !window_active {
        return false;
    }
    let adj = ctx.scroll.vadjustment();
    let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.value() >= max_val - 4.0
}

/// Refresh the floating sticky header: find the finished block whose vertical
/// span contains the viewport's top edge and show its command there. Hidden when
/// the top edge is over the active card, an empty area, or in fullscreen.
fn update_sticky_header(ctx: &Rc<Ctx>) {
    if ctx.fullscreen.get() {
        ctx.sticky_header.set_visible(false);
        return;
    }
    let top = ctx.scroll.vadjustment().value();
    let finished = ctx.finished.borrow();
    let mut found: Option<usize> = None;
    for (idx, block) in finished.iter().enumerate() {
        if !block.widget.get_visible() {
            continue;
        }
        let Some(p) = block
            .widget
            .compute_point(&ctx.block_list, &gtk::graphene::Point::new(0.0, 0.0))
        else {
            continue;
        };
        let y = p.y() as f64;
        let h = block.widget.height() as f64;
        // Header shows only once a block's top has scrolled above the viewport
        // edge (i.e. its command line is no longer visible).
        if y < top && top < y + h {
            found = Some(idx);
        }
    }
    match found {
        Some(idx) => {
            let block = &finished[idx];
            let label = if block.command.is_empty() {
                block.prompt.clone()
            } else {
                block.command.clone()
            };
            ctx.sticky_label.set_text(&label);
            ctx.sticky_header.remove_css_class("sticky-ok");
            ctx.sticky_header.remove_css_class("sticky-bad");
            if block.exit_code == 0 {
                ctx.sticky_header.add_css_class("sticky-ok");
            } else {
                ctx.sticky_header.add_css_class("sticky-bad");
            }
            ctx.sticky_idx.set(Some(idx));
            ctx.sticky_header.set_visible(true);
        }
        None => {
            ctx.sticky_idx.set(None);
            ctx.sticky_header.set_visible(false);
        }
    }
}

fn scroll_to_block(ctx: &Rc<Ctx>, idx: usize) {
    let finished = ctx.finished.borrow();
    let Some(block) = finished.get(idx) else { return };
    if let Some(p) = block
        .widget
        .compute_point(&ctx.block_list, &gtk::graphene::Point::new(0.0, 0.0))
    {
        let adj = ctx.scroll.vadjustment();
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        animate_scroll_to(ctx, (p.y() as f64).clamp(adj.lower(), max_val));
    }
}

/// Smoothly scroll the block list to `target` (an absolute vadjustment value)
/// with an ease-out tween. A generation guard cancels any in-flight animation.
fn animate_scroll_to(ctx: &Rc<Ctx>, target: f64) {
    let adj = ctx.scroll.vadjustment();
    let start = adj.value();
    let dist = target - start;
    if dist.abs() < 1.0 {
        adj.set_value(target);
        return;
    }
    let gen = ctx.scroll_anim_gen.get().wrapping_add(1);
    ctx.scroll_anim_gen.set(gen);
    let begin = Instant::now();
    const DUR_MS: f64 = 180.0;
    let ctx = ctx.clone();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        // A newer animation (or a teardown) supersedes this one.
        if ctx.scroll_anim_gen.get() != gen {
            return glib::ControlFlow::Break;
        }
        let t = (begin.elapsed().as_secs_f64() * 1000.0 / DUR_MS).min(1.0);
        // Cubic ease-out.
        let eased = 1.0 - (1.0 - t).powi(3);
        ctx.scroll.vadjustment().set_value(start + dist * eased);
        if t >= 1.0 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Cycle to the next/previous detected error line within the selected block,
/// expanding its output first, and scroll that line into view. No-op when no
/// block is selected or the selected block has no detected errors.
fn jump_to_error(ctx: &Rc<Ctx>, delta: i32) {
    let Some(sel) = ctx.selected_block.get() else { return };
    let err_bg = error_highlight_bg(ctx);
    let finished = ctx.finished.borrow();
    let Some(block) = finished.get(sel) else { return };
    if block.error_offsets.is_empty() {
        return;
    }
    // Ensure the full output is rendered + visible so every error is reachable.
    if let Some(view) = &block.output_view {
        if block.truncated.get() || block.collapsed.get() {
            render_block_output(view, &block.full_runs, &block.full_runs, false, &block.error_offsets, &err_bg);
            block.truncated.set(false);
            block.collapsed.set(false);
            view.set_visible(true);
            if let Some(b) = &block.show_more {
                b.set_visible(false);
            }
        }
    }
    let n = block.error_offsets.len() as i32;
    let cur = block.error_idx.get() as i32;
    let next = ((cur + delta) % n + n) % n;
    block.error_idx.set(next as usize);
    let off = block.error_offsets[next as usize];
    scroll_to_offset(ctx, block, off);
}

/// Scroll the outer list so the given char offset inside a block's output view is
/// near the top of the viewport.
fn scroll_to_offset(ctx: &Rc<Ctx>, block: &FinishedBlock, offset: i32) {
    let Some(view) = &block.output_view else { return };
    let iter = view.buffer().iter_at_offset(offset);
    let loc = view.iter_location(&iter);
    let (_wx, wy) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, loc.x(), loc.y());
    if let Some(p) = view.compute_point(&ctx.block_list, &gtk::graphene::Point::new(0.0, wy as f32)) {
        let adj = ctx.scroll.vadjustment();
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        let target = (p.y() as f64 - 8.0).clamp(adj.lower(), max_val);
        animate_scroll_to(ctx, target);
    }
}

/// Background color string for error-line highlight, derived from palette red.
fn error_highlight_bg(ctx: &Rc<Ctx>) -> String {
    let err = ctx.config.borrow().palette[1];
    format!(
        "rgba({},{},{},0.28)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    )
}

// ─── Reader event handling ──────────────────────────────────────────────────

fn handle_data(ctx: &Rc<Ctx>, sender: &ComponentSender<BlockTerminal>, data: &[u8]) {
    let mut events = Vec::new();
    ctx.parser.borrow_mut().feed(data, &mut events);
    for ev in events {
        handle_event(ctx, sender, ev);
    }
    update_active_height(ctx);
    autoscroll(ctx);
}

fn handle_event(ctx: &Rc<Ctx>, sender: &ComponentSender<BlockTerminal>, ev: ParserEvent) {
    match ev {
        ParserEvent::Bytes(bytes) => {
            if contains_bell(&bytes) {
                let _ = sender.output(VteOutput::Bell);
            }
            if let Some(title) = scan_title(&bytes) {
                let _ = sender.output(VteOutput::TitleChanged(title));
            }
            // Idle (no integration yet): treat as raw fallback once real output flows.
            if ctx.state.get() == BlockState::Idle {
                ctx.state.set(BlockState::RawFallback);
            }
            match ctx.state.get() {
                BlockState::CollectingPrompt => {
                    ctx.prompt_buf.borrow_mut().extend_from_slice(&bytes)
                }
                BlockState::AwaitingCommand => ctx.cmd_buf.borrow_mut().extend_from_slice(&bytes),
                BlockState::CollectingOutput => {
                    ctx.out_buf.borrow_mut().extend_from_slice(&bytes);
                    let _ = sender.output(VteOutput::Activity);
                }
                BlockState::AltScreen => {
                    // A pager that repaints a fresh page first clears the screen;
                    // snapshot the page currently on the grid before the clear lands
                    // so paged-through content (e.g. `git log` over many commits) is
                    // preserved instead of being overwritten.
                    if super::alt::contains_clear_screen(&bytes) {
                        super::alt::record_pager_snapshot(
                            &ctx.active_vte,
                            &ctx.pager_snapshots,
                            &ctx.pager_preclear,
                        );
                    }
                    // Also snapshot the rendered frame after the feed below paints it,
                    // so the finished block keeps the pager's content on exit.
                    super::alt::schedule_pager_snapshot(
                        &ctx.active_vte,
                        &ctx.pager_snapshots,
                        &ctx.pager_generation,
                        &ctx.pager_preclear,
                    );
                }
                _ => {}
            }
            ctx.active_vte.feed(&bytes);
        }
        ParserEvent::PromptStart => {
            // Finalize the previous command (deferred from its CommandEnd).
            if ctx.has_command.get() {
                finalize_block(ctx);
            }
            ctx.state.set(BlockState::CollectingPrompt);
        }
        ParserEvent::PromptEnd => {
            // Snapshot the rendered prompt (last non-empty line) for Copy/export.
            let prompt_line = strip_ansi(&ctx.prompt_buf.borrow())
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            *ctx.prompt.borrow_mut() = prompt_line;
            ctx.prompt_buf.borrow_mut().clear();
            ctx.cmd_buf.borrow_mut().clear();
            ctx.typed_cmd.borrow_mut().clear();
            ctx.state.set(BlockState::AwaitingCommand);
        }
        ParserEvent::CommandStart => {
            ctx.out_buf.borrow_mut().clear();
            ctx.start_time.set(Some(Instant::now()));
            ctx.has_command.set(true);
            ctx.state.set(BlockState::CollectingOutput);
        }
        ParserEvent::CommandEnd(code) => {
            ctx.exit_code.set(code);
            let elapsed = ctx.start_time.get().map(|t| t.elapsed());
            ctx.duration.set(elapsed);
            ctx.end_time_ms.set(Some(now_ms()));
            ctx.state.set(BlockState::PostCommand);

            // In-app completion notice: only when the command was slow enough to
            // matter AND the user isn't watching (tab inactive or scrolled away
            // from the bottom). Routed to the existing tab-highlight mechanism.
            let slow = elapsed.map(|d| d.as_millis() as u64).unwrap_or(0) >= NOTIFY_THRESHOLD_MS;
            let off_screen = slow && !user_is_watching(ctx);
            ctx.pending_toast.set(off_screen);
            if off_screen {
                let _ = sender.output(VteOutput::CommandFinished(code == 0));
            }
        }
        ParserEvent::CwdUpdate(path) => {
            *ctx.cwd.borrow_mut() = path.clone();
            let _ = sender.output(VteOutput::CwdChanged(path));
        }
        ParserEvent::AltScreenEnter => {
            ctx.prev_state.set(ctx.state.get());
            ctx.state.set(BlockState::AltScreen);
            // Cancel any snapshot scheduled for the previous alt-screen session
            // and baseline the current frame so its stale render is dropped.
            ctx.pager_generation.set(ctx.pager_generation.get().wrapping_add(1));
            ctx.pager_snapshots.borrow_mut().clear();
            // Normalize the baseline so it is comparable to the normalized frames
            // captured later; otherwise the stale pre-alt prompt line leaks in as
            // the first "page" of the recorded output.
            *ctx.pager_preclear.borrow_mut() = super::alt::normalize_pager_snapshot(
                &super::alt::visible_vte_text(&ctx.active_vte),
            );
            // Give the alt-screen app the full viewport: hide the finished blocks
            // so the active card fills the scroll area, matching a normal
            // terminal. Restored on leave.
            enter_fullscreen(ctx);
            ctx.active_vte.feed(b"\x1b[?1049h");
        }
        ParserEvent::AltScreenLeave => {
            // Capture the final visible frame synchronously *before* switching back
            // to the normal buffer. The deferred idle captures race with the VTE's
            // paint and frequently never land (leaving an empty block), so this is
            // the reliable snapshot of the app's last screen.
            super::alt::record_pager_snapshot(
                &ctx.active_vte,
                &ctx.pager_snapshots,
                &ctx.pager_preclear,
            );
            ctx.active_vte.feed(b"\x1b[?1049l");
            // Bump the generation so no late idle capture lands after we drain.
            ctx.pager_generation.set(ctx.pager_generation.get().wrapping_add(1));
            let merged = super::alt::drain_pager_snapshots(&ctx.pager_snapshots);
            if !merged.is_empty() {
                let mut out = ctx.out_buf.borrow_mut();
                if !out.is_empty() && !out.ends_with(b"\n") {
                    out.push(b'\n');
                }
                out.extend_from_slice(merged.as_bytes());
            }
            exit_fullscreen(ctx);
            ctx.state.set(ctx.prev_state.get());
        }
        ParserEvent::ClipboardSet(text) => {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
        ParserEvent::ApcSequence(_) => {}
    }
}

/// Compact minimum height for the active (input) card, in text rows. Keeps the
/// prompt + an input line visible without ballooning to the full screen.
const MIN_ACTIVE_ROWS: i64 = 3;

/// Count the number of visual rows `bytes` occupy when rendered at `cols`
/// columns, counting line wraps. ANSI escape sequences and UTF-8 continuation
/// bytes are skipped so the width estimate is reasonable. Scanning stops once
/// `cap` rows are reached, bounding the cost for huge output.
fn count_wrapped_rows(bytes: &[u8], cols: i64, cap: i64) -> i64 {
    let cols = cols.max(1);
    let mut rows: i64 = 0;
    let mut col: i64 = 0;
    let mut esc = false;
    for &b in bytes {
        if esc {
            // Crude CSI/escape skip: terminates on a final byte (ASCII letter).
            if b.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        match b {
            0x1b => esc = true,
            b'\n' => {
                rows += 1;
                col = 0;
            }
            b'\r' => col = 0,
            // Advance one cell per character, ignoring UTF-8 continuation bytes.
            b if b >= 0x20 && (b & 0xc0) != 0x80 => {
                col += 1;
                if col >= cols {
                    rows += 1;
                    col = 0;
                }
            }
            _ => {}
        }
        if rows >= cap {
            return cap;
        }
    }
    rows + if col > 0 { 1 } else { 0 }
}

/// Resize the active card to fit its current content, clamped between a compact
/// input minimum and the viewport height. Alt-screen apps and no-OSC133 shells
/// get the full viewport (they behave as a normal full-screen terminal). This
/// keeps the live input compact with history stacked above (warp-style) while
/// letting command output expand the card as it streams.
fn update_active_height(ctx: &Rc<Ctx>) {
    let page_px = ctx.scroll.vadjustment().page_size();
    if page_px <= 1.0 {
        return;
    }
    let ch = ctx.active_vte.char_height();
    if ch <= 1 {
        return; // terminal not realized yet
    }
    let page_rows = (page_px as i64 / ch).max(1);

    let target_rows = if ctx.fullscreen.get() || ctx.state.get() == BlockState::RawFallback {
        page_rows
    } else {
        let cols = ctx.active_vte.column_count().max(1);
        let cmd_rows = count_wrapped_rows(ctx.typed_cmd.borrow().as_bytes(), cols, page_rows);
        let content = match ctx.state.get() {
            BlockState::CollectingOutput | BlockState::PostCommand => {
                1 + cmd_rows + count_wrapped_rows(&ctx.out_buf.borrow(), cols, page_rows)
            }
            _ => 1 + cmd_rows,
        };
        content.clamp(MIN_ACTIVE_ROWS, page_rows)
    };
    ctx.active_holder
        .set_height_request((target_rows * ch) as i32);
}

fn autoscroll(ctx: &Rc<Ctx>) {
    if ctx.fullscreen.get() || !ctx.stick_bottom.get() {
        return;
    }
    let adj = ctx.scroll.vadjustment();
    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
}

/// Hand the viewport to an alt-screen app: hide every finished block so the
/// active card fills the scroll area like a normal full-screen terminal. The
/// active VTE's row/column count then matches the window, so the PTY resize tick
/// reports the full size to the app.
fn enter_fullscreen(ctx: &Rc<Ctx>) {
    if ctx.fullscreen.replace(true) {
        return;
    }
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(false);
    }
    ctx.sticky_header.set_visible(false);
}

/// Restore the block list when the alt-screen app exits, re-applying the active
/// filter so hidden-by-filter blocks stay hidden.
fn exit_fullscreen(ctx: &Rc<Ctx>) {
    if !ctx.fullscreen.replace(false) {
        return;
    }
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(block_visible(ctx, block));
    }
    rebuild_minimap(ctx);
}

/// Snapshot the current command + output into a finished block, then reset the
/// active card for the next command.
fn finalize_block(ctx: &Rc<Ctx>) {
    // Prefer the keystroke-reconstructed command; fall back to scraping the last
    // line of the echoed output (e.g. for history recall / paste).
    let typed = ctx.typed_cmd.borrow().trim().to_string();
    let command = if !typed.is_empty() {
        typed
    } else {
        strip_ansi(&ctx.cmd_buf.borrow())
            .lines()
            .next_back()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    if command.is_empty() {
        // Nothing meaningful to record; just reset.
        reset_active(ctx);
        return;
    }
    let output = String::from_utf8_lossy(&ctx.out_buf.borrow()).into_owned();
    let exit_code = ctx.exit_code.get();
    let cwd = ctx.cwd.borrow().clone();
    let prompt = ctx.prompt.borrow().clone();
    let duration = ctx.duration.get();
    let end_time_ms = ctx.end_time_ms.get();
    let id = ctx.next_block_id.get();
    ctx.next_block_id.set(id + 1);

    let meta = build_finished_block(
        ctx, id, &prompt, &command, &output, exit_code, &cwd, duration, end_time_ms, false,
    );
    let widget = meta.widget.clone();
    let duration_ms = meta.duration_ms;
    ctx.block_list.append(&widget);
    ctx.block_list
        .reorder_child_after(&ctx.active_holder, Some(&widget));

    widget.set_visible(block_visible(ctx, &meta));
    ctx.finished.borrow_mut().push(meta);

    append_block_history(ctx, &prompt, &command, &output, exit_code, &cwd, duration_ms, end_time_ms, false);

    let new_idx = ctx.finished.borrow().len() - 1;
    pulse_block(ctx, new_idx, exit_code == 0);
    // On failure, surface the first error if the user is at the bottom watching.
    if exit_code != 0 && user_is_watching(ctx) {
        scroll_to_first_error(ctx, new_idx);
    }
    // Off-screen slow completion: raise a click-to-jump toast.
    if ctx.pending_toast.replace(false) {
        show_toast(ctx, new_idx, &command, duration, exit_code == 0);
    }

    enforce_max_blocks(ctx);
    rebuild_minimap(ctx);
    reset_active(ctx);
}

/// One persisted finished block. Stores raw (ANSI-bearing) output so a reloaded
/// block renders identically to when it was first produced.
#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryRecord {
    #[serde(default)]
    prompt: String,
    command: String,
    output: String,
    exit_code: i32,
    cwd: String,
    duration_ms: u64,
    #[serde(default)]
    end_time_ms: Option<u64>,
    #[serde(default)]
    pinned: bool,
}

/// Export shape mirroring jterm4's `BlockData` (field names + order), so the
/// clipboard JSON/Markdown produced by the context menu matches jterm4's.
#[derive(serde::Serialize)]
struct BlockExport {
    id: u64,
    prompt: String,
    cmd: String,
    output: String,
    exit_code: i32,
    estimated_height: i32,
    line_count: usize,
    start_time_ms: Option<u64>,
    end_time_ms: Option<u64>,
    duration_ms: Option<u64>,
    cwd: Option<String>,
}

impl BlockExport {
    fn from_block(b: &FinishedBlock) -> Self {
        let duration_ms = if b.duration_ms > 0 { Some(b.duration_ms) } else { None };
        let start_time_ms = match (b.end_time_ms, duration_ms) {
            (Some(end), Some(dur)) => Some(end.saturating_sub(dur)),
            _ => None,
        };
        BlockExport {
            id: b.id,
            prompt: b.prompt.clone(),
            cmd: b.command.clone(),
            output: b.plain_output.clone(),
            exit_code: b.exit_code,
            estimated_height: 0,
            line_count: b.plain_output.lines().count(),
            start_time_ms,
            end_time_ms: b.end_time_ms,
            duration_ms,
            cwd: if b.cwd.is_empty() { None } else { Some(b.cwd.clone()) },
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str("## Command Block\n\n");
        if !self.prompt.is_empty() {
            md.push_str(&format!("**Prompt:** `{}`\n\n", self.prompt));
        }
        md.push_str("**Command:**\n```bash\n");
        md.push_str(&self.cmd);
        md.push_str("\n```\n\n");
        if !self.output.is_empty() {
            md.push_str("**Output:**\n```\n");
            md.push_str(&self.output);
            md.push_str("\n```\n\n");
        }
        md.push_str(&format!("**Exit Code:** {}\n\n", self.exit_code));
        if let Some(dur) = self.duration_ms {
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur as f64 / 1000.0));
        }
        md
    }
}

/// Append a finished block to the configured history file (JSON Lines). No-op
/// when `block_history_path` is unset. `block_history_compress` is not honored
/// in jterm1 (jterm4's rkyv+zstd format is not portable here); records are
/// written as plain newline-delimited JSON.
#[allow(clippy::too_many_arguments)]
fn append_block_history(
    ctx: &Rc<Ctx>,
    prompt: &str,
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
    duration_ms: u64,
    end_time_ms: Option<u64>,
    pinned: bool,
) {
    let Some(path) = ctx.config.borrow().block_history_path.clone() else {
        return;
    };
    let record = HistoryRecord {
        prompt: prompt.to_string(),
        command: command.to_string(),
        output: output.to_string(),
        exit_code,
        cwd: cwd.to_string(),
        duration_ms,
        end_time_ms,
        pinned,
    };
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            let _ = writeln!(file, "{line}");
        }
        Err(err) => log::warn!("Failed to append block history to {path}: {err}"),
    }
}

/// Render the tail of the persisted history into the block list at startup, so a
/// fresh session resumes with prior finished blocks visible.
fn load_block_history(ctx: &Rc<Ctx>) {
    const MAX_RESTORED: usize = 200;
    let Some(path) = ctx.config.borrow().block_history_path.clone() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(MAX_RESTORED);
    for line in &lines[start..] {
        let Ok(rec) = serde_json::from_str::<HistoryRecord>(line) else {
            continue;
        };
        let duration = if rec.duration_ms > 0 {
            Some(Duration::from_millis(rec.duration_ms))
        } else {
            None
        };
        let id = ctx.next_block_id.get();
        ctx.next_block_id.set(id + 1);
        let meta = build_finished_block(
            ctx,
            id,
            &rec.prompt,
            &rec.command,
            &rec.output,
            rec.exit_code,
            &rec.cwd,
            duration,
            rec.end_time_ms,
            rec.pinned,
        );
        let widget = meta.widget.clone();
        ctx.block_list.append(&widget);
        ctx.block_list
            .reorder_child_after(&ctx.active_holder, Some(&widget));
        widget.set_visible(block_visible(ctx, &meta));
        ctx.finished.borrow_mut().push(meta);
    }
    enforce_max_blocks(ctx);
}

fn passes_filter(filter: BlockFilter, block: &FinishedBlock) -> bool {
    match filter {
        BlockFilter::None => true,
        // Pinned blocks stay visible under any content filter.
        BlockFilter::Failed => block.pinned || block.exit_code != 0,
        BlockFilter::Slow => block.pinned || block.duration_ms >= SLOW_THRESHOLD_MS,
        BlockFilter::Pinned => block.pinned,
    }
}

/// Whether a block should be visible: it must pass the preset filter AND, if a
/// live command-filter query is set, contain that query (case-insensitive).
fn block_visible(ctx: &Rc<Ctx>, block: &FinishedBlock) -> bool {
    if !passes_filter(ctx.filter.get(), block) {
        return false;
    }
    let query = ctx.filter_query.borrow();
    if query.trim().is_empty() {
        return true;
    }
    block.command.to_lowercase().contains(&query.to_lowercase())
}

/// Re-apply visibility to every finished block per the current preset + live
/// filter, then refresh the minimap and index badges.
fn apply_visibility(ctx: &Rc<Ctx>) {
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(block_visible(ctx, block));
    }
    rebuild_minimap(ctx);
    refresh_index_badges(ctx);
}

/// Enforce `max_visible_blocks`: evict the oldest non-pinned finished blocks once
/// the live count exceeds the cap (0 = unlimited). History on disk is unaffected.
fn enforce_max_blocks(ctx: &Rc<Ctx>) {
    let max = ctx.config.borrow().max_visible_blocks as usize;
    if max == 0 {
        return;
    }
    let mut evicted = false;
    loop {
        let len = ctx.finished.borrow().len();
        if len <= max {
            break;
        }
        let pos = ctx.finished.borrow().iter().position(|b| !b.pinned);
        let Some(pos) = pos else { break };
        let block = ctx.finished.borrow_mut().remove(pos);
        ctx.block_list.remove(&block.widget);
        evicted = true;
    }
    if evicted {
        select_block(ctx, None);
        ctx.search_matches.borrow_mut().clear();
        ctx.search_idx.set(0);
    }
}

/// Set the active filter and toggle each finished block's visibility.
fn apply_filter(ctx: &Rc<Ctx>, filter: BlockFilter) {
    ctx.filter.set(filter);
    select_block(ctx, None);
    apply_visibility(ctx);
}

/// Move the keyboard block-selection highlight to `target` (an index into
/// `finished`, or `None` to clear). Swaps the `.block-selected` css class and
/// scrolls the newly selected block into view. Mirrors jterm4's block nav.
fn select_block(ctx: &Rc<Ctx>, target: Option<usize>) {
    let finished = ctx.finished.borrow();
    if let Some(old) = ctx.selected_block.get() {
        if let Some(block) = finished.get(old) {
            block.widget.remove_css_class("block-selected");
        }
    }
    let target = target.filter(|&i| i < finished.len());
    if let Some(idx) = target {
        if let Some(block) = finished.get(idx) {
            block.widget.add_css_class("block-selected");
        }
    }
    ctx.selected_block.set(target);
    drop(finished);
    ctx.hint_bar.set_visible(target.is_some());
    refresh_index_badges(ctx);
    if let Some(idx) = target {
        scroll_to_block(ctx, idx);
    }
}

/// Indices into `finished` of the currently visible blocks, in display order.
fn visible_indices(ctx: &Rc<Ctx>) -> Vec<usize> {
    ctx.finished
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, b)| b.widget.is_visible())
        .map(|(i, _)| i)
        .collect()
}

/// Select the first (or last) currently-visible block.
fn jump_block_edge(ctx: &Rc<Ctx>, first: bool) {
    let visible = visible_indices(ctx);
    let target = if first { visible.first() } else { visible.last() };
    if let Some(&idx) = target {
        select_block(ctx, Some(idx));
    }
}

/// Select the `n`th (0-based) currently-visible block, clamped to the last.
fn jump_to_nth_visible(ctx: &Rc<Ctx>, n: usize) {
    let visible = visible_indices(ctx);
    if visible.is_empty() {
        return;
    }
    let idx = visible[n.min(visible.len() - 1)];
    select_block(ctx, Some(idx));
}

/// Step the block selection by `delta` (+1 = next/down, -1 = prev/up) over the
/// currently *visible* finished blocks, clamping at the ends. With no current
/// selection, Up selects the last visible block and Down selects the first.
fn step_block_selection(ctx: &Rc<Ctx>, delta: i32) {
    let visible = visible_indices(ctx);
    if visible.is_empty() {
        return;
    }
    let cur = ctx.selected_block.get();
    let pos_in_visible = cur.and_then(|c| visible.iter().position(|&i| i == c));
    let next_idx = match pos_in_visible {
        None => {
            if delta < 0 {
                visible.len() - 1
            } else {
                0
            }
        }
        Some(p) => {
            let np = p as i32 + delta;
            np.clamp(0, visible.len() as i32 - 1) as usize
        }
    };
    select_block(ctx, Some(visible[next_idx]));
}

/// Clear all visible finished blocks from the list and drop their metadata, then
/// send a form feed to the shell (Ctrl+L). Mirrors jterm4's Ctrl+L behavior.
fn clear_visible_blocks(ctx: &Rc<Ctx>) {
    select_block(ctx, None);
    let mut finished = ctx.finished.borrow_mut();
    for block in finished.drain(..) {
        ctx.block_list.remove(&block.widget);
    }
    drop(finished);
    ctx.search_matches.borrow_mut().clear();
    ctx.search_idx.set(0);
    rebuild_minimap(ctx);
    ctx.pty.write_bytes(b"\x0c");
}

fn reset_active(ctx: &Rc<Ctx>) {
    ctx.active_vte.reset(true, true);
    // `reset()` acts on the emulator state immediately, but `feed()` bytes are
    // parsed asynchronously: the just-finished command's output is still queued
    // and would replay onto the cleared grid, leaving stale lines above the next
    // prompt. Feed an in-stream clear (home + erase screen + erase scrollback) so
    // it is ordered *after* that queued output and wipes it.
    ctx.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
    ctx.cmd_buf.borrow_mut().clear();
    ctx.typed_cmd.borrow_mut().clear();
    ctx.out_buf.borrow_mut().clear();
    ctx.has_command.set(false);
    ctx.exit_code.set(0);
    ctx.duration.set(None);
    ctx.end_time_ms.set(None);
    ctx.start_time.set(None);
}

// ─── Finished-block widget ──────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_finished_block(
    ctx: &Rc<Ctx>,
    id: u64,
    prompt: &str,
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
    duration: Option<Duration>,
    end_time_ms: Option<u64>,
    pinned: bool,
) -> FinishedBlock {
    let outer = gtk::Box::new(Orientation::Vertical, 0);
    outer.add_css_class("block-finished");
    if exit_code == 0 {
        outer.add_css_class("block-success");
    } else {
        outer.add_css_class("block-failed");
    }
    if pinned {
        outer.add_css_class("block-pinned");
    }
    outer.set_hexpand(true);

    // Parse ANSI output into styled runs once; `plain_output` is the de-styled
    // text used for the empty check and clipboard copy.
    let palette = ctx.config.borrow().palette;
    let runs: Rc<Vec<AnsiTextRun>> = Rc::new(ansi::ansi_text_runs(output, &palette));
    let plain_output: String = runs.iter().map(|r| r.text.as_str()).collect();

    // Detect error-line offsets (only meaningful for failed commands).
    let error_offsets = if exit_code != 0 {
        detect_error_offsets(&plain_output)
    } else {
        Vec::new()
    };

    // Header row.
    let header = gtk::Box::new(Orientation::Horizontal, 6);
    header.add_css_class("block-header");

    // Index badge (1-9): hidden until block-selection mode numbers it.
    let index_badge = gtk::Label::new(None);
    index_badge.add_css_class("block-index-badge");
    index_badge.set_visible(false);
    header.append(&index_badge);
    let index_badge_ret = index_badge.clone();

    // Status icon: Nerd Font check () on success, times () on failure.
    let status = gtk::Label::new(Some(if exit_code == 0 { "\u{f00c}" } else { "\u{f00d}" }));
    status.add_css_class(if exit_code == 0 {
        "block-status-ok"
    } else {
        "block-status-bad"
    });
    header.append(&status);

    // Pin indicator (Nerd Font thumbtack ), shown only when the block is pinned.
    let pin_icon = gtk::Label::new(Some("\u{f08d}"));
    pin_icon.add_css_class("block-pin-icon");
    pin_icon.set_visible(pinned);
    header.append(&pin_icon);
    let pin_icon_ret = pin_icon.clone();

    let cwd_label = gtk::Label::new(Some(&shorten_path(cwd)));
    cwd_label.add_css_class("block-header-label");
    cwd_label.set_xalign(0.0);
    header.append(&cwd_label);

    let spacer = gtk::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);

    // Relative timestamp ("2m ago"), refreshed by the periodic timer; the exact
    // wall-clock time is available on hover.
    let time_label = end_time_ms.map(|et| {
        let ts = gtk::Label::new(Some(&format_relative(et)));
        ts.add_css_class("block-header-label");
        ts.set_tooltip_text(Some(&format_clock(et)));
        header.append(&ts);
        ts
    });

    if let Some(d) = duration {
        let badge = gtk::Label::new(Some(&format_duration(d)));
        badge.add_css_class("block-meta-badge");
        badge.add_css_class(duration_grade_class(d));
        header.append(&badge);
    }

    if exit_code != 0 {
        let exit_badge = gtk::Label::new(Some(&format!("exit:{exit_code}")));
        exit_badge.add_css_class("block-exit-bad");
        header.append(&exit_badge);
    }

    // Action buttons: copy command, copy output, rerun. Hidden until the block
    // is hovered. Nerd Font glyphs: copy (), clipboard (), refresh ().
    let action_box = gtk::Box::new(Orientation::Horizontal, 2);
    action_box.set_visible(false);

    let copy_cmd = gtk::Button::with_label("\u{f0c5}");
    copy_cmd.add_css_class("block-action-btn");
    copy_cmd.add_css_class("flat");
    copy_cmd.set_tooltip_text(Some("Copy command"));
    {
        let cmd = command.to_string();
        copy_cmd.connect_clicked(move |_| set_clipboard(&cmd));
    }
    action_box.append(&copy_cmd);

    let copy_out = gtk::Button::with_label("\u{f0ea}");
    copy_out.add_css_class("block-action-btn");
    copy_out.add_css_class("flat");
    copy_out.set_tooltip_text(Some("Copy output"));
    {
        let out = plain_output.clone();
        copy_out.connect_clicked(move |_| set_clipboard(&out));
    }
    action_box.append(&copy_out);

    let rerun = gtk::Button::with_label("\u{f021}");
    rerun.add_css_class("block-action-btn");
    rerun.add_css_class("flat");
    rerun.set_tooltip_text(Some("Rerun command"));
    {
        let pty = ctx.pty.clone();
        let cmd = command.to_string();
        rerun.connect_clicked(move |_| {
            pty.write_bytes(format!("{cmd}\r").as_bytes());
        });
    }
    action_box.append(&rerun);

    // Copy error report (failed blocks only): command + exit code + error lines,
    // formatted for pasting into an AI assistant or bug report. Nerd Font bug ().
    if exit_code != 0 {
        let copy_err = gtk::Button::with_label("\u{f188}");
        copy_err.add_css_class("block-action-btn");
        copy_err.add_css_class("block-action-err");
        copy_err.add_css_class("flat");
        copy_err.set_tooltip_text(Some("Copy error report"));
        {
            let report = build_error_report(command, cwd, exit_code, &plain_output);
            copy_err.connect_clicked(move |_| set_clipboard(&report));
        }
        action_box.append(&copy_err);
    }

    header.append(&action_box);

    // Collapse toggle: chevron-down () expanded, chevron-right () collapsed.
    let collapse_btn = gtk::Button::with_label("\u{f078}");
    collapse_btn.add_css_class("block-collapse-btn");
    collapse_btn.add_css_class("flat");
    collapse_btn.set_tooltip_text(Some("Collapse output"));
    header.append(&collapse_btn);

    outer.append(&header);

    // Reveal action buttons + highlight on hover.
    let hover = gtk::EventControllerMotion::new();
    {
        let outer = outer.clone();
        let action_box = action_box.clone();
        hover.connect_enter(move |_, _, _| {
            outer.add_css_class("block-hovered");
            action_box.set_visible(true);
        });
    }
    {
        let outer = outer.clone();
        let action_box = action_box.clone();
        hover.connect_leave(move |_| {
            outer.remove_css_class("block-hovered");
            action_box.set_visible(false);
        });
    }
    outer.add_controller(hover);

    // Command view.
    let command_view = plain_text_view(command, "block-command-view");
    url::attach_url_handlers(&command_view);
    outer.append(&command_view);

    // Background for error-line highlight, derived from the palette red.
    let err_bg = error_highlight_bg(ctx);
    let error_offsets_rc: Rc<Vec<i32>> = Rc::new(error_offsets.clone());

    // Output view with reversible truncation + lazy rendering. `truncated` tracks
    // whether the head (vs. full output) is shown; `collapsed` whether the output
    // area is hidden; `rendered` guards lazy first-render.
    let truncate_lines = ctx.config.borrow().max_collapsed_output_lines as usize;
    let lazy_threshold = ctx.config.borrow().lazy_load_threshold as usize;
    let total_lines = ansi::count_lines(&runs);
    let do_truncate = truncate_lines > 0 && total_lines > truncate_lines;
    // Failed blocks always render eagerly + expanded so the error is on screen.
    let lazy = lazy_threshold > 0 && total_lines > lazy_threshold && exit_code == 0;
    let has_output = !plain_output.is_empty();

    let collapsed = Rc::new(Cell::new(!has_output || lazy));
    let truncated = Rc::new(Cell::new(do_truncate));
    let rendered = Rc::new(Cell::new(false));

    let head_runs: Rc<Vec<AnsiTextRun>> = if do_truncate {
        let head_chars = ansi::char_offset_after_lines(&runs, truncate_lines);
        Rc::new(ansi::truncate_runs(&runs, head_chars))
    } else {
        runs.clone()
    };

    let mut output_view: Option<gtk::TextView> = None;
    let mut show_more: Option<gtk::Button> = None;
    if has_output {
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        view.add_css_class("block-output-view");
        url::attach_url_handlers(&view);
        view.set_visible(!collapsed.get());
        // Render eagerly unless this block starts collapsed (lazy/no-output);
        // collapsed blocks render on first expand.
        if !collapsed.get() {
            render_block_output(&view, &head_runs, &runs, truncated.get(), &error_offsets_rc, &err_bg);
            rendered.set(true);
        }
        outer.append(&view);

        if do_truncate {
            let hidden = total_lines - truncate_lines;
            let btn = gtk::Button::with_label(&format!("▼ show {hidden} more lines"));
            btn.add_css_class("block-show-more");
            btn.set_halign(gtk::Align::Start);
            btn.set_visible(!collapsed.get());
            {
                let view = view.clone();
                let head_runs = head_runs.clone();
                let full = runs.clone();
                let truncated = truncated.clone();
                let errors = error_offsets_rc.clone();
                let err_bg = err_bg.clone();
                btn.connect_clicked(move |btn| {
                    let now_truncated = !truncated.get();
                    truncated.set(now_truncated);
                    render_block_output(&view, &head_runs, &full, now_truncated, &errors, &err_bg);
                    let label = if now_truncated {
                        format!("▼ show {hidden} more lines")
                    } else {
                        "▲ show less".to_string()
                    };
                    btn.set_label(&label);
                });
            }
            outer.append(&btn);
            show_more = Some(btn);
        }
        output_view = Some(view);
    }

    // Wire the collapse chevron to toggle the output area. Blocks that start
    // collapsed (no output / lazy) render their content on first expand.
    {
        let output_view = output_view.clone();
        let show_more = show_more.clone();
        let collapsed = collapsed.clone();
        let rendered = rendered.clone();
        let truncated = truncated.clone();
        let head_runs = head_runs.clone();
        let full = runs.clone();
        let errors = error_offsets_rc.clone();
        let err_bg = err_bg.clone();
        let do_truncate_c = do_truncate;
        collapse_btn.connect_clicked(move |btn| {
            let now_collapsed = !collapsed.get();
            collapsed.set(now_collapsed);
            if !now_collapsed && !rendered.get() {
                if let Some(v) = &output_view {
                    render_block_output(v, &head_runs, &full, truncated.get(), &errors, &err_bg);
                }
                rendered.set(true);
            }
            if let Some(v) = &output_view {
                v.set_visible(!now_collapsed);
            }
            if let Some(b) = &show_more {
                b.set_visible(!now_collapsed && do_truncate_c);
            }
            btn.set_label(if now_collapsed { "\u{f054}" } else { "\u{f078}" });
        });
    }
    if collapsed.get() {
        collapse_btn.set_label("\u{f054}");
    }

    // Right-click context menu: Copy Block / Export JSON / Export Markdown / Delete.
    {
        let right_click = gtk::GestureClick::new();
        right_click.set_button(3);
        let ctx = ctx.clone();
        let outer_for_menu = outer.clone();
        right_click.connect_pressed(move |gesture, _n, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_block_menu(&ctx, id, &outer_for_menu, x, y);
        });
        outer.add_controller(right_click);
    }

    let duration_ms = duration.map(|d| d.as_millis() as u64).unwrap_or(0);
    FinishedBlock {
        id,
        widget: outer,
        command_view,
        output_view,
        show_more,
        full_runs: runs,
        collapsed,
        truncated,
        pinned,
        error_offsets,
        error_idx: Cell::new(0),
        pin_icon: pin_icon_ret,
        index_badge: index_badge_ret,
        collapse_btn,
        time_label,
        prompt: prompt.to_string(),
        command: command.to_string(),
        plain_output,
        exit_code,
        cwd: cwd.to_string(),
        duration_ms,
        end_time_ms,
    }
}

/// Render the output view with either the truncated head or the full runs, then
/// re-apply the error-line highlight (offsets that fall outside the rendered text
/// are skipped).
fn render_block_output(
    view: &gtk::TextView,
    head_runs: &[AnsiTextRun],
    full_runs: &[AnsiTextRun],
    truncated: bool,
    error_offsets: &[i32],
    err_bg: &str,
) {
    let runs = if truncated { head_runs } else { full_runs };
    render_ansi_runs(view, runs);
    apply_line_highlight(view, error_offsets, "jterm-error", err_bg);
}

/// Apply a named background tag from each given char offset to its line end.
fn apply_line_highlight(view: &gtk::TextView, offsets: &[i32], tag_name: &str, bg: &str) {
    if offsets.is_empty() {
        return;
    }
    let buffer = view.buffer();
    let table = buffer.tag_table();
    let tag = table.lookup(tag_name).unwrap_or_else(|| {
        let t = gtk::TextTag::builder().name(tag_name).background(bg).build();
        table.add(&t);
        t
    });
    let char_count = buffer.char_count();
    for &off in offsets {
        if off >= char_count {
            continue;
        }
        let start = buffer.iter_at_offset(off);
        let mut end = start;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        buffer.apply_tag(&tag, &start, &end);
    }
}

/// Scan plain output for lines that look like errors, returning the char offset
/// of the start of each such line (for failed-block error highlighting + n/N).
fn detect_error_offsets(plain: &str) -> Vec<i32> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(error|errors|panic|traceback|fatal|exception|failed|failure)\b")
            .expect("valid error regex")
    });
    let mut offsets = Vec::new();
    let mut char_off: i32 = 0;
    for line in plain.split_inclusive('\n') {
        if re.is_match(line) {
            offsets.push(char_off);
        }
        char_off += line.chars().count() as i32;
    }
    offsets
}

/// Build and pop up the per-block right-click context menu. Uses a plain
/// `Popover` + flat buttons (mirrors jterm4, whose GAction menu path is broken in
/// this GTK build). Menu actions look the block up by stable `id`.
fn show_block_menu(ctx: &Rc<Ctx>, id: u64, anchor: &gtk::Box, x: f64, y: f64) {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));

    let vbox = gtk::Box::new(Orientation::Vertical, 0);
    vbox.add_css_class("menu");

    let make_item = |label: &str| -> gtk::Button {
        let btn = gtk::Button::with_label(label);
        btn.set_has_frame(false);
        btn.set_halign(gtk::Align::Fill);
        if let Some(child) = btn.child() {
            child.set_halign(gtk::Align::Start);
        }
        btn.add_css_class("flat");
        btn
    };

    let (is_pinned, is_failed) = ctx
        .finished
        .borrow()
        .iter()
        .find(|b| b.id == id)
        .map(|b| (b.pinned, b.exit_code != 0))
        .unwrap_or((false, false));
    let pin_label = if is_pinned { "Unpin Block" } else { "Pin Block" };
    let mut items: Vec<(&str, fn(&Rc<Ctx>, u64))> = vec![
        (pin_label, toggle_pin),
        ("Copy Block", copy_block_by_id),
    ];
    if is_failed {
        items.push(("Copy Error Report", copy_error_report_by_id));
    }
    items.extend([
        ("Export as JSON", export_block_json as fn(&Rc<Ctx>, u64)),
        ("Export as Markdown", export_block_markdown),
        ("Delete Block", delete_block),
    ]);
    for (label, action) in items {
        let item = make_item(label);
        let ctx = ctx.clone();
        let popover_c = popover.clone();
        item.connect_clicked(move |_| {
            popover_c.popdown();
            action(&ctx, id);
        });
        vbox.append(&item);
    }

    popover.set_child(Some(&vbox));
    popover.connect_closed(|p| p.unparent());
    popover.popup();
}

/// Toggle a block's pinned state: update its widget styling + pin glyph, keep it
/// visible if the current filter would hide it, and persist the change.
fn toggle_pin(ctx: &Rc<Ctx>, id: u64) {
    let mut changed: Option<(String, Option<u64>, bool)> = None;
    {
        let mut finished = ctx.finished.borrow_mut();
        if let Some(b) = finished.iter_mut().find(|b| b.id == id) {
            b.pinned = !b.pinned;
            if b.pinned {
                b.widget.add_css_class("block-pinned");
            } else {
                b.widget.remove_css_class("block-pinned");
            }
            b.pin_icon.set_visible(b.pinned);
            b.widget.set_visible(block_visible(ctx, b));
            changed = Some((b.command.clone(), b.end_time_ms, b.pinned));
        }
    }
    if let Some((command, end_time_ms, pinned)) = changed {
        update_history_pin(ctx, &command, end_time_ms, pinned);
    }
}

/// Rewrite the persisted history file to reflect a pin toggle, matching the
/// record by command + end time. No-op when no history file is configured.
fn update_history_pin(ctx: &Rc<Ctx>, command: &str, end_time_ms: Option<u64>, pinned: bool) {
    let Some(path) = ctx.config.borrow().block_history_path.clone() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        match serde_json::from_str::<HistoryRecord>(line) {
            Ok(mut rec) if rec.command == command && rec.end_time_ms == end_time_ms => {
                rec.pinned = pinned;
                if let Ok(s) = serde_json::to_string(&rec) {
                    out.push_str(&s);
                } else {
                    out.push_str(line);
                }
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if let Err(err) = std::fs::write(&path, out) {
        log::warn!("Failed to persist pin state to {path}: {err}");
    }
}

/// Copy a finished block (prompt + command + output) to the clipboard.
fn copy_block_by_id(ctx: &Rc<Ctx>, id: u64) {
    if let Some(b) = ctx.finished.borrow().iter().find(|b| b.id == id) {
        set_clipboard(&format!("{}\n{}\n{}", b.prompt, b.command, b.plain_output));
    }
}

fn export_block_json(ctx: &Rc<Ctx>, id: u64) {
    if let Some(b) = ctx.finished.borrow().iter().find(|b| b.id == id) {
        set_clipboard(&BlockExport::from_block(b).to_json());
    }
}

fn export_block_markdown(ctx: &Rc<Ctx>, id: u64) {
    if let Some(b) = ctx.finished.borrow().iter().find(|b| b.id == id) {
        set_clipboard(&BlockExport::from_block(b).to_markdown());
    }
}

/// Remove a finished block from the list. Index-based caches (search results,
/// selection) are reset since deletion shifts positions.
fn delete_block(ctx: &Rc<Ctx>, id: u64) {
    let mut finished = ctx.finished.borrow_mut();
    if let Some(pos) = finished.iter().position(|b| b.id == id) {
        let block = finished.remove(pos);
        ctx.block_list.remove(&block.widget);
    }
    drop(finished);
    select_block(ctx, None);
    ctx.search_matches.borrow_mut().clear();
    ctx.search_idx.set(0);
    rebuild_minimap(ctx);
}

fn ansi_output_view(runs: &[AnsiTextRun], css_class: &str) -> gtk::TextView {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    view.add_css_class(css_class);
    render_ansi_runs(&view, runs);
    url::attach_url_handlers(&view);
    view
}

fn render_ansi_runs(view: &gtk::TextView, runs: &[AnsiTextRun]) {
    let buffer = view.buffer();
    let text: String = runs.iter().map(|r| r.text.as_str()).collect();
    buffer.set_text(&text);
    ansi::apply_ansi_runs_to_buffer(&buffer, 0, runs);
}

fn plain_text_view(text: &str, css_class: &str) -> gtk::TextView {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    view.add_css_class(css_class);
    view.buffer().set_text(text);
    view
}

fn set_clipboard(text: &str) {
    if let Some(display) = gtk::gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences (CSI/OSC/charset) and CRs, leaving plain text.
fn strip_ansi(input: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            0x1b => {
                i += 1;
                if i >= input.len() {
                    break;
                }
                match input[i] {
                    b'[' => {
                        i += 1;
                        while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                            i += 1;
                        }
                        i += 1; // final byte
                    }
                    b']' => {
                        i += 1;
                        while i < input.len() {
                            if input[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if input[i] == 0x1b
                                && i + 1 < input.len()
                                && input[i + 1] == b'\\'
                            {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    b'(' | b')' => i += 2,
                    _ => i += 1,
                }
            }
            b'\r' => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True if `bytes` contains a BEL that is not an OSC string terminator.
fn contains_bell(bytes: &[u8]) -> bool {
    let mut in_osc = false;
    let mut prev_esc = false;
    for &b in bytes {
        if in_osc {
            // OSC ends on BEL (0x07) or ST (ESC \).
            if b == 0x07 || (prev_esc && b == b'\\') {
                in_osc = false;
            }
            prev_esc = b == 0x1b;
            continue;
        }
        match b {
            0x07 => return true,
            0x1b => prev_esc = true,
            b']' if prev_esc => {
                in_osc = true;
                prev_esc = false;
            }
            _ => prev_esc = false,
        }
    }
    false
}

/// Extract a window title from an OSC 0/2 sequence, if present.
fn scan_title(bytes: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && bytes[j] != 0x07 && bytes[j] != 0x1b {
                j += 1;
            }
            let payload = &bytes[start..j];
            if let Some(rest) = payload
                .strip_prefix(b"0;")
                .or_else(|| payload.strip_prefix(b"2;"))
            {
                let title = String::from_utf8_lossy(rest).into_owned();
                if !title.is_empty() {
                    return Some(title);
                }
            }
            i = j;
        }
        i += 1;
    }
    None
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Local timezone offset from UTC in seconds (matches jterm4's helper, used to
/// render block timestamps in local time without pulling in `chrono`).
fn chrono_local_offset_secs() -> i64 {
    use nix::libc;
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

/// Format a wall-clock epoch-ms value as local `HH:MM:SS`.
fn format_clock(end_time_ms: u64) -> String {
    let secs = end_time_ms / 1000;
    let local = (secs as i64 + chrono_local_offset_secs()).rem_euclid(86400) as u64;
    format!("{:02}:{:02}:{:02}", local / 3600, (local % 3600) / 60, local % 60)
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        let secs = d.as_secs();
        if secs < 60 {
            format!("{:.1}s", d.as_secs_f64())
        } else {
            format!("{}m{}s", secs / 60, secs % 60)
        }
    }
}

// ─── CSS (ported from jterm4 block_view/css.rs) ─────────────────────────────

fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    let parts: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        display
    } else {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    }
}

// ─── UX helpers (badges, minimap, toasts, pulses, relative time) ────────────

/// Number the first nine visible blocks 1-9 while in block-selection mode; hide
/// every badge when no block is selected.
fn refresh_index_badges(ctx: &Rc<Ctx>) {
    let active = ctx.selected_block.get().is_some();
    let finished = ctx.finished.borrow();
    let mut n = 0u32;
    for block in finished.iter() {
        if active && block.widget.is_visible() && n < 9 {
            n += 1;
            block.index_badge.set_text(&n.to_string());
            block.index_badge.set_visible(true);
        } else {
            block.index_badge.set_visible(false);
        }
    }
}

/// Rebuild the right-edge minimap: one colored tick per visible block, clickable
/// to jump. Hidden in fullscreen or when fewer than two blocks are visible.
fn rebuild_minimap(ctx: &Rc<Ctx>) {
    while let Some(child) = ctx.minimap.first_child() {
        ctx.minimap.remove(&child);
    }
    let visible = visible_indices(ctx);
    if ctx.fullscreen.get() || visible.len() < 2 {
        ctx.minimap.set_visible(false);
        return;
    }
    let finished = ctx.finished.borrow();
    for &idx in &visible {
        let Some(block) = finished.get(idx) else { continue };
        let tick = gtk::Button::new();
        tick.add_css_class("block-minimap-tick");
        tick.add_css_class("flat");
        if block.pinned {
            tick.add_css_class("tick-pinned");
        } else if block.exit_code == 0 {
            tick.add_css_class("tick-ok");
        } else {
            tick.add_css_class("tick-bad");
        }
        let short: String = block.command.chars().take(60).collect();
        tick.set_tooltip_text(Some(&short));
        let ctx2 = ctx.clone();
        tick.connect_clicked(move |_| scroll_to_block(&ctx2, idx));
        ctx.minimap.append(&tick);
    }
    ctx.minimap.set_visible(true);
}

/// Briefly flash a just-finished block (green on success, red on failure) for
/// peripheral awareness, then drop the pulse class.
fn pulse_block(ctx: &Rc<Ctx>, idx: usize, ok: bool) {
    if ctx.fullscreen.get() {
        return;
    }
    let finished = ctx.finished.borrow();
    let Some(block) = finished.get(idx) else { return };
    let widget = block.widget.clone();
    let class = if ok { "block-pulse-ok" } else { "block-pulse-bad" };
    widget.add_css_class(class);
    glib::timeout_add_local_once(Duration::from_millis(700), move || {
        widget.remove_css_class(class);
    });
}

/// Expand a failed block's full output and scroll its first detected error line
/// into view, without changing the keyboard selection. The scroll is deferred so
/// the freshly-appended widget has been allocated.
fn scroll_to_first_error(ctx: &Rc<Ctx>, idx: usize) {
    let err_bg = error_highlight_bg(ctx);
    {
        let finished = ctx.finished.borrow();
        let Some(block) = finished.get(idx) else { return };
        if block.error_offsets.is_empty() {
            return;
        }
        if let Some(view) = &block.output_view {
            if block.truncated.get() || block.collapsed.get() {
                render_block_output(view, &block.full_runs, &block.full_runs, false, &block.error_offsets, &err_bg);
                block.truncated.set(false);
                block.collapsed.set(false);
                view.set_visible(true);
                if let Some(b) = &block.show_more {
                    b.set_visible(false);
                }
            }
        }
    }
    let ctx2 = ctx.clone();
    glib::timeout_add_local_once(Duration::from_millis(30), move || {
        let finished = ctx2.finished.borrow();
        if let Some(block) = finished.get(idx) {
            if let Some(&off) = block.error_offsets.first() {
                scroll_to_offset(&ctx2, block, off);
            }
        }
    });
}

/// Show a click-to-jump toast (bottom-right) announcing an off-screen completion;
/// auto-dismisses after a few seconds.
fn show_toast(ctx: &Rc<Ctx>, idx: usize, command: &str, duration: Option<Duration>, ok: bool) {
    let id = match ctx.finished.borrow().get(idx) {
        Some(b) => b.id,
        None => return,
    };
    let glyph = if ok { "\u{f00c}" } else { "\u{f00d}" };
    let dur = duration
        .map(|d| format!("  ·  {}", format_duration(d)))
        .unwrap_or_default();
    let short: String = command.chars().take(48).collect();
    let btn = gtk::Button::with_label(&format!("{glyph}  {short}{dur}"));
    btn.add_css_class("block-toast");
    btn.add_css_class(if ok { "toast-ok" } else { "toast-bad" });
    btn.add_css_class("flat");
    {
        let ctx2 = ctx.clone();
        let btn_c = btn.clone();
        btn.connect_clicked(move |_| {
            let pos = ctx2.finished.borrow().iter().position(|b| b.id == id);
            if let Some(p) = pos {
                scroll_to_block(&ctx2, p);
            }
            ctx2.toast_box.remove(&btn_c);
        });
    }
    ctx.toast_box.append(&btn);
    let toast_box = ctx.toast_box.clone();
    let btn_c = btn.clone();
    glib::timeout_add_local_once(Duration::from_secs(6), move || {
        if btn_c.parent().is_some() {
            toast_box.remove(&btn_c);
        }
    });
}

/// Toggle the collapsed state of the currently-selected block.
fn toggle_selected_collapse(ctx: &Rc<Ctx>) {
    if let Some(i) = ctx.selected_block.get() {
        if let Some(b) = ctx.finished.borrow().get(i) {
            b.collapse_btn.emit_clicked();
        }
    }
}

/// Collapse (or expand) every block with output. Reuses each block's collapse
/// button so lazy first-render and label state stay consistent.
fn set_all_collapsed(ctx: &Rc<Ctx>, want: bool) {
    for b in ctx.finished.borrow().iter() {
        if b.output_view.is_some() && b.collapsed.get() != want {
            b.collapse_btn.emit_clicked();
        }
    }
}

/// Pop a keyboard cheatsheet over the block list.
fn show_cheatsheet(ctx: &Rc<Ctx>) {
    let popover = gtk::Popover::new();
    popover.set_parent(&ctx.scroll);
    let text = "Block navigation\n\n\
        Shift+Up/Down    select / enter nav\n\
        j / k            move selection\n\
        gg / G           first / last block\n\
        1-9              jump to block N\n\
        n / N            next / prev error\n\
        Enter            recall command\n\
        y                copy block\n\
        Space            fold / unfold block\n\
        , / .            fold / unfold all\n\
        /                filter by command\n\
        Shift+PgUp/PgDn  page the list\n\
        Esc              exit nav";
    let label = gtk::Label::new(Some(text));
    label.add_css_class("block-cheatsheet");
    label.set_xalign(0.0);
    popover.set_child(Some(&label));
    popover.connect_closed(|p| p.unparent());
    popover.popup();
}

fn copy_error_report_by_id(ctx: &Rc<Ctx>, id: u64) {
    if let Some(b) = ctx.finished.borrow().iter().find(|b| b.id == id) {
        set_clipboard(&build_error_report(&b.command, &b.cwd, b.exit_code, &b.plain_output));
    }
}

/// Format a copy-ready error report (command, cwd, exit code, output tail) for
/// pasting into an assistant or bug report.
fn build_error_report(command: &str, cwd: &str, exit_code: i32, output: &str) -> String {
    const TAIL: usize = 80;
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(TAIL);
    let elision = if start > 0 { "…(earlier output omitted)…\n" } else { "" };
    let tail = lines[start..].join("\n");
    format!("$ {command}\n# cwd: {cwd}\n# exit code: {exit_code}\n\n{elision}{tail}")
}

/// Refresh every block's relative-time label ("2m ago").
fn refresh_relative_times(ctx: &Rc<Ctx>) {
    for b in ctx.finished.borrow().iter() {
        if let (Some(label), Some(et)) = (&b.time_label, b.end_time_ms) {
            label.set_text(&format_relative(et));
        }
    }
}

/// Render a wall-clock epoch-ms value as a short relative time, falling back to
/// the absolute clock for timestamps older than a week or in the future.
fn format_relative(end_time_ms: u64) -> String {
    let now = now_ms();
    if end_time_ms == 0 || end_time_ms > now {
        return format_clock(end_time_ms);
    }
    let secs = (now - end_time_ms) / 1000;
    if secs < 10 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 604800 {
        format!("{}d ago", secs / 86400)
    } else {
        format_clock(end_time_ms)
    }
}

/// CSS class grading the duration badge by speed, so slow commands stand out.
fn duration_grade_class(d: Duration) -> &'static str {
    let ms = d.as_millis() as u64;
    if ms < 500 {
        "dur-fast"
    } else if ms < SLOW_THRESHOLD_MS {
        "dur-normal"
    } else if ms < 5000 {
        "dur-slow"
    } else {
        "dur-veryslow"
    }
}

#[allow(deprecated)]
fn install_block_css(config: &Config) {
    let fg = &config.foreground;
    let bg = &config.background;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    let cursor_hex = rgba_to_hex(&config.cursor);
    let accent = rgba_to_hex(&config.palette[2]);
    let err = &config.palette[1];
    let err_hex = rgba_to_hex(err);
    let err_bg = format!(
        "rgba({},{},{},0.18)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );

    let ok = &config.palette[2];
    let ok_stripe = format!(
        "rgba({},{},{},0.55)",
        (ok.red() * 255.0) as u8,
        (ok.green() * 255.0) as u8,
        (ok.blue() * 255.0) as u8,
    );
    let ok_hex = rgba_to_hex(ok);
    let err_stripe = format!(
        "rgba({},{},{},0.70)",
        (err.red() * 255.0) as u8,
        (err.green() * 255.0) as u8,
        (err.blue() * 255.0) as u8,
    );

    let ok_r = (ok.red() * 255.0) as u8;
    let ok_g = (ok.green() * 255.0) as u8;
    let ok_b = (ok.blue() * 255.0) as u8;
    let err_r = (err.red() * 255.0) as u8;
    let err_g = (err.green() * 255.0) as u8;
    let err_b = (err.blue() * 255.0) as u8;
    let acc = &config.palette[2];
    let acc_r = (acc.red() * 255.0) as u8;
    let acc_g = (acc.green() * 255.0) as u8;
    let acc_b = (acc.blue() * 255.0) as u8;

    // Amber/warn tone for the "slow" duration grade, from the palette yellow.
    let warn = &config.palette[3];
    let warn_r = (warn.red() * 255.0) as u8;
    let warn_g = (warn.green() * 255.0) as u8;
    let warn_b = (warn.blue() * 255.0) as u8;
    let warn_hex = rgba_to_hex(warn);

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    let bg_r = (bg.red() * 255.0) as u8;
    let bg_g = (bg.green() * 255.0) as u8;
    let bg_b = (bg.blue() * 255.0) as u8;
    let block_bg_hex = format!(
        "#{:02x}{:02x}{:02x}",
        (bg_r as f32 + (fg_r as f32 - bg_r as f32) * 0.03) as u8,
        (bg_g as f32 + (fg_g as f32 - bg_g as f32) * 0.03) as u8,
        (bg_b as f32 + (fg_b as f32 - bg_b as f32) * 0.03) as u8,
    );

    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (font_family, base_size) = if parts.len() >= 2 {
        if let Ok(size) = parts[parts.len() - 1].parse::<f64>() {
            let family = parts[..parts.len() - 1].join(" ");
            (family, size.round().max(1.0) as i32)
        } else {
            (config.font_desc.clone(), 14)
        }
    } else {
        (config.font_desc.clone(), 14)
    };
    let font_family = font_family.replace('\\', "\\\\").replace('"', "\\\"");
    let scaled_size = (base_size as f64 * config.default_font_scale).round().max(1.0) as i32;
    let font_size = format!("{}pt", scaled_size);

    let css = format!(
        r#"
        .block-scroll {{ background-color: {bg_hex}; }}
        .block-list {{ background-color: {bg_hex}; }}
        .block-finished {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.08);
            border-left: 3px solid transparent;
            border-radius: 10px;
            background-color: {block_bg_hex};
            margin: 4px 8px;
            min-height: 40px;
            transition: background-color 140ms ease, border-color 140ms ease, box-shadow 140ms ease;
        }}
        .block-success {{ border-left-color: {ok_stripe}; }}
        .block-failed {{ border-left-color: {err_stripe}; }}
        .block-hovered {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.05);
            box-shadow: 0 4px 14px rgba(0,0,0,0.22);
        }}
        .block-selected {{
            border-color: {accent};
            box-shadow: 0 0 0 1px {accent}, 0 4px 14px rgba(0,0,0,0.28);
        }}
        .block-active {{
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.22);
            border-left: 3px solid {accent};
            border-radius: 10px;
            margin: 6px 8px;
            padding-top: 4px;
            padding-bottom: 4px;
            background-color: {block_bg_hex};
            min-height: 40px;
            transition: box-shadow 140ms ease, border-color 140ms ease;
        }}
        .block-active:focus-within {{
            border-color: {accent};
            box-shadow: 0 0 0 1px rgba({acc_r},{acc_g},{acc_b},0.45), 0 6px 18px rgba(0,0,0,0.30);
        }}
        .block-status-ok {{
            color: {ok_hex};
            background-color: rgba({ok_r},{ok_g},{ok_b},0.16);
            border-radius: 999px;
            min-width: 16px; min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em; font-weight: bold;
            margin-left: 8px;
        }}
        .block-status-bad {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.18);
            border-radius: 999px;
            min-width: 16px; min-height: 16px;
            padding: 1px 5px;
            font-family: "{font_family}";
            font-size: 0.82em; font-weight: bold;
            margin-left: 8px;
        }}
        .block-action-btn {{
            color: {dim_fg};
            min-width: 24px; min-height: 24px;
            padding: 0 4px;
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.9em;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-action-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-collapse-btn {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.8em;
            min-width: 24px; min-height: 24px;
            padding: 0;
            border-radius: 999px;
            transition: background-color 120ms ease, color 120ms ease;
        }}
        .block-collapse-btn:hover {{
            color: {fg_hex};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.12);
        }}
        .block-header {{
            border-radius: 6px 6px 0 0;
            padding: 2px 6px;
        }}
        .block-header-label {{ color: {dim_fg}; font-size: 0.85em; }}
        .block-command-view {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0 12px;
            background-color: {block_bg_hex};
            caret-color: {cursor_hex};
        }}
        .block-command-view text {{ color: {fg_hex}; background-color: {block_bg_hex}; }}
        .block-output-view {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0 12px 4px 12px;
            background-color: {block_bg_hex};
        }}
        .block-output-view text {{ color: {fg_hex}; background-color: {block_bg_hex}; }}
        .block-exit-bad {{
            color: {err_hex};
            background-color: {err_bg};
            border: 1px solid rgba({err_r},{err_g},{err_b},0.35);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em; font-weight: bold;
            padding: 1px 8px;
        }}
        .block-meta-badge {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 999px;
            font-family: "{font_family}";
            font-size: 0.78em;
            padding: 1px 8px;
        }}
        .block-show-more {{
            color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.25);
            border-radius: 999px;
            margin-left: 12px; margin-top: 6px; margin-bottom: 4px;
            font-size: 0.82em;
            padding: 2px 12px;
            transition: background-color 120ms ease;
        }}
        .block-show-more:hover {{ background-color: rgba({acc_r},{acc_g},{acc_b},0.18); }}
        .block-pinned {{
            border-left-color: {accent};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.06);
        }}
        .block-pin-icon {{
            color: {accent};
            font-family: "{font_family}";
            font-size: 0.82em;
            margin-left: 6px;
        }}
        .block-sticky-header {{
            background-color: rgba({bg_r},{bg_g},{bg_b},0.92);
            border-bottom: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-left: 3px solid {accent};
            padding: 4px 12px;
            margin: 0;
        }}
        .block-sticky-header.sticky-bad {{ border-left-color: {err_stripe}; }}
        .block-sticky-header.sticky-ok {{ border-left-color: {ok_stripe}; }}
        .block-sticky-label {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
        }}
        .block-index-badge {{
            color: {bg_hex};
            background-color: {accent};
            border-radius: 999px;
            min-width: 15px; min-height: 15px;
            padding: 0 5px;
            margin-left: 6px;
            font-family: "{font_family}";
            font-size: 0.74em; font-weight: bold;
        }}
        .block-action-err {{ color: {err_hex}; }}
        .block-action-err:hover {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.16);
        }}
        .block-meta-badge.dur-fast {{ color: {dim_fg}; opacity: 0.7; }}
        .block-meta-badge.dur-normal {{ color: {dim_fg}; }}
        .block-meta-badge.dur-slow {{
            color: {warn_hex};
            background-color: rgba({warn_r},{warn_g},{warn_b},0.14);
        }}
        .block-meta-badge.dur-veryslow {{
            color: {err_hex};
            background-color: rgba({err_r},{err_g},{err_b},0.16);
            font-weight: bold;
        }}
        @keyframes block-pulse-ok-kf {{
            0% {{ background-color: rgba({ok_r},{ok_g},{ok_b},0.32); }}
            100% {{ background-color: {block_bg_hex}; }}
        }}
        @keyframes block-pulse-bad-kf {{
            0% {{ background-color: rgba({err_r},{err_g},{err_b},0.34); }}
            100% {{ background-color: {block_bg_hex}; }}
        }}
        .block-pulse-ok {{ animation: block-pulse-ok-kf 700ms ease-out; }}
        .block-pulse-bad {{ animation: block-pulse-bad-kf 700ms ease-out; }}
        .block-hint-bar {{
            background-color: rgba({bg_r},{bg_g},{bg_b},0.92);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.14);
            border-radius: 999px;
            margin-bottom: 12px;
            padding: 3px 14px;
            box-shadow: 0 3px 12px rgba(0,0,0,0.30);
        }}
        .block-hint-label {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: 0.80em;
        }}
        .block-toast-box {{ margin: 0 14px 14px 0; }}
        .block-toast {{
            color: {fg_hex};
            background-color: rgba({bg_r},{bg_g},{bg_b},0.96);
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.16);
            border-left: 3px solid {accent};
            border-radius: 8px;
            padding: 6px 14px;
            font-family: "{font_family}";
            font-size: 0.84em;
            box-shadow: 0 4px 16px rgba(0,0,0,0.34);
        }}
        .block-toast.toast-ok {{ border-left-color: {ok_stripe}; }}
        .block-toast.toast-bad {{ border-left-color: {err_stripe}; }}
        .block-toast:hover {{ background-color: rgba({fg_r},{fg_g},{fg_b},0.10); }}
        .block-minimap {{
            margin: 6px 14px 6px 3px;
            min-width: 6px;
            opacity: 0.5;
            transition: opacity 140ms ease;
        }}
        .block-minimap:hover {{ opacity: 1.0; }}
        .block-minimap-tick {{
            min-width: 6px; min-height: 4px;
            padding: 0;
            border-radius: 3px;
            background-color: rgba({fg_r},{fg_g},{fg_b},0.25);
        }}
        .block-minimap-tick.tick-ok {{ background-color: {ok_stripe}; }}
        .block-minimap-tick.tick-bad {{ background-color: {err_stripe}; }}
        .block-minimap-tick.tick-pinned {{ background-color: {accent}; }}
        .block-minimap-tick:hover {{ background-color: {fg_hex}; }}
        .block-filter-entry {{
            margin: 6px 8px;
            background-color: {block_bg_hex};
            color: {fg_hex};
            font-family: "{font_family}";
        }}
        .block-cheatsheet {{
            font-family: "{font_family}";
            font-size: 0.84em;
            padding: 6px 4px;
        }}
        "#,
    );

    thread_local! {
        static BLOCK_CSS_PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    }

    let provider = gtk::CssProvider::new();
    provider.load_from_data(&css);
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    BLOCK_CSS_PROVIDER.with(|cell| {
        let mut prev = cell.borrow_mut();
        if let Some(old) = prev.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *prev = Some(provider);
    });
}
