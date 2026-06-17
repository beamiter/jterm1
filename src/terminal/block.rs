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
}

/// Metadata + widget for one finished block, used by filtering, breadcrumb,
/// the right-click context menu, and export.
struct FinishedBlock {
    /// Stable identity (monotonic), so context-menu closures can find this block
    /// after deletions have shifted vector positions.
    id: u64,
    widget: gtk::Box,
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
    /// Finished blocks in display order (top→bottom), for filtering + breadcrumb.
    finished: RefCell<Vec<FinishedBlock>>,
    filter: Cell<BlockFilter>,
    breadcrumb: gtk::Button,
    /// Index into `finished` of the block the breadcrumb currently names.
    breadcrumb_target: Cell<Option<usize>>,
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

        // Floating breadcrumb: names the command whose output fills the viewport
        // top once it has scrolled past. Overlaid on the scroll area.
        let breadcrumb = gtk::Button::with_label("");
        breadcrumb.add_css_class("block-breadcrumb");
        breadcrumb.set_halign(gtk::Align::Fill);
        breadcrumb.set_valign(gtk::Align::Start);
        breadcrumb.set_visible(false);
        if let Some(lbl) = breadcrumb.child().and_downcast::<gtk::Label>() {
            lbl.set_xalign(0.0);
            lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }

        let overlay = gtk::Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        overlay.set_child(Some(&scroll));
        overlay.add_overlay(&breadcrumb);
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
            breadcrumb: breadcrumb.clone(),
            breadcrumb_target: Cell::new(None),
            search_matches: RefCell::new(Vec::new()),
            search_idx: Cell::new(0),
            selected_block: Cell::new(None),
            pager_snapshots: Rc::new(RefCell::new(Vec::new())),
            pager_generation: Rc::new(Cell::new(0)),
            pager_preclear: Rc::new(RefCell::new(String::new())),
            fullscreen: Cell::new(false),
        });

        // Track which finished block fills the viewport top → breadcrumb.
        // Update on both value and range changes: while blocks stream in, the
        // adjustment's `upper` grows after the final value-changed fires (the
        // active card relayouts), shifting visible content without moving value.
        {
            let ctx = ctx.clone();
            scroll
                .vadjustment()
                .connect_value_changed(move |adj| update_breadcrumb(&ctx, adj.value()));
        }
        {
            let ctx = ctx.clone();
            scroll
                .vadjustment()
                .connect_changed(move |adj| update_breadcrumb(&ctx, adj.value()));
        }
        // Clicking the breadcrumb jumps to the top of the block it names.
        {
            let ctx = ctx.clone();
            breadcrumb.connect_clicked(move |_| {
                if let Some(idx) = ctx.breadcrumb_target.get() {
                    if let Some(block) = ctx.finished.borrow().get(idx) {
                        let adj = ctx.scroll.vadjustment();
                        if let Some(p) = block.widget.compute_point(
                            &ctx.scroll,
                            &gtk::graphene::Point::new(0.0, 0.0),
                        ) {
                            adj.set_value(adj.value() + p.y() as f64);
                        }
                    }
                }
            });
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

        // Track size changes and resize the PTY accordingly.
        {
            let pty = pty.clone();
            let last = Rc::new(Cell::new((0i64, 0i64)));
            active_vte.add_tick_callback(move |term, _clock| {
                let cols = term.column_count();
                let rows = term.row_count();
                if (cols, rows) != last.get() && cols > 0 && rows > 0 {
                    last.set((cols, rows));
                    pty.resize(cols as u16, rows as u16);
                }
                glib::ControlFlow::Continue
            });
        }

        // Restore previously-persisted finished blocks (if history is configured).
        load_block_history(&ctx);

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
            VteInput::ClearBlockFilter => apply_filter(&self.ctx, BlockFilter::None),
            VteInput::SearchSet(query, use_regex) => search_set(&self.ctx, &query, use_regex),
            VteInput::SearchNext => search_step(&self.ctx, 1),
            VteInput::SearchPrev => search_step(&self.ctx, -1),
            VteInput::SearchClear => {
                self.ctx.search_matches.borrow_mut().clear();
                self.ctx.search_idx.set(0);
            }
        }
    }
}

/// Compute the set of finished blocks matching `query` and jump to the first.
fn search_set(ctx: &Rc<Ctx>, query: &str, use_regex: bool) {
    let mut matches = Vec::new();
    let re = if use_regex {
        regex::RegexBuilder::new(query)
            .case_insensitive(true)
            .build()
            .ok()
    } else {
        None
    };
    let needle = query.to_lowercase();
    for (idx, block) in ctx.finished.borrow().iter().enumerate() {
        let hay = format!("{}\n{}", block.command, block.plain_output);
        let hit = match &re {
            Some(re) => re.is_match(&hay),
            None => hay.to_lowercase().contains(&needle),
        };
        if hit {
            matches.push(idx);
        }
    }
    *ctx.search_matches.borrow_mut() = matches;
    ctx.search_idx.set(0);
    if let Some(&first) = ctx.search_matches.borrow().first() {
        scroll_to_block(ctx, first);
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
fn scroll_to_block(ctx: &Rc<Ctx>, idx: usize) {
    let finished = ctx.finished.borrow();
    let Some(block) = finished.get(idx) else { return };
    if let Some(p) = block
        .widget
        .compute_point(&ctx.block_list, &gtk::graphene::Point::new(0.0, 0.0))
    {
        let adj = ctx.scroll.vadjustment();
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        adj.set_value((p.y() as f64).clamp(adj.lower(), max_val));
    }
}

// ─── Reader event handling ──────────────────────────────────────────────────

fn handle_data(ctx: &Rc<Ctx>, sender: &ComponentSender<BlockTerminal>, data: &[u8]) {
    let mut events = Vec::new();
    ctx.parser.borrow_mut().feed(data, &mut events);
    for ev in events {
        handle_event(ctx, sender, ev);
    }
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
                        eprintln!("[altdbg] clear-screen in stream → record before clear");
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
            ctx.duration.set(ctx.start_time.get().map(|t| t.elapsed()));
            ctx.end_time_ms.set(Some(now_ms()));
            ctx.state.set(BlockState::PostCommand);
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
            eprintln!("[altdbg] ENTER: pre_clear baseline =\n===\n{}\n===", ctx.pager_preclear.borrow());
            // Give the alt-screen app the full viewport: hide the finished blocks
            // (and breadcrumb) so the active card fills the scroll area, matching a
            // normal terminal. Restored on leave.
            enter_fullscreen(ctx);
            ctx.active_vte.feed(b"\x1b[?1049h");
        }
        ParserEvent::AltScreenLeave => {
            // Capture the final visible frame synchronously *before* switching back
            // to the normal buffer. The deferred idle captures race with the VTE's
            // paint and frequently never land (leaving an empty block), so this is
            // the reliable snapshot of the app's last screen.
            eprintln!("[altdbg] LEAVE: capturing final frame");
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

fn autoscroll(ctx: &Rc<Ctx>) {
    if ctx.fullscreen.get() {
        return;
    }
    let adj = ctx.scroll.vadjustment();
    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
}

/// Hand the viewport to an alt-screen app: hide every finished block and the
/// breadcrumb so the active card (which is `vexpand`) fills the scroll area like
/// a normal full-screen terminal. The active VTE's row/column count then matches
/// the window, so the PTY resize tick reports the full size to the app.
fn enter_fullscreen(ctx: &Rc<Ctx>) {
    if ctx.fullscreen.replace(true) {
        return;
    }
    ctx.breadcrumb.set_visible(false);
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(false);
    }
}

/// Restore the block list when the alt-screen app exits, re-applying the active
/// filter so hidden-by-filter blocks stay hidden.
fn exit_fullscreen(ctx: &Rc<Ctx>) {
    if !ctx.fullscreen.replace(false) {
        return;
    }
    let filter = ctx.filter.get();
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(passes_filter(filter, block));
    }
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

    let block = build_finished_block(ctx, id, &command, &output, exit_code, &cwd, duration, end_time_ms);
    ctx.block_list.append(&block);
    ctx.block_list
        .reorder_child_after(&ctx.active_holder, Some(&block));

    let duration_ms = duration.map(|d| d.as_millis() as u64).unwrap_or(0);
    let meta = FinishedBlock {
        id,
        widget: block.clone(),
        prompt: prompt.clone(),
        command: command.clone(),
        plain_output: strip_ansi(output.as_bytes()),
        exit_code,
        cwd: cwd.clone(),
        duration_ms,
        end_time_ms,
    };
    block.set_visible(passes_filter(ctx.filter.get(), &meta));
    ctx.finished.borrow_mut().push(meta);

    append_block_history(ctx, &prompt, &command, &output, exit_code, &cwd, duration_ms, end_time_ms);

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
        let block = build_finished_block(
            ctx,
            id,
            &rec.command,
            &rec.output,
            rec.exit_code,
            &rec.cwd,
            duration,
            rec.end_time_ms,
        );
        ctx.block_list.append(&block);
        ctx.block_list
            .reorder_child_after(&ctx.active_holder, Some(&block));
        let meta = FinishedBlock {
            id,
            widget: block.clone(),
            prompt: rec.prompt.clone(),
            command: rec.command.clone(),
            plain_output: strip_ansi(rec.output.as_bytes()),
            exit_code: rec.exit_code,
            cwd: rec.cwd.clone(),
            duration_ms: rec.duration_ms,
            end_time_ms: rec.end_time_ms,
        };
        block.set_visible(passes_filter(ctx.filter.get(), &meta));
        ctx.finished.borrow_mut().push(meta);
    }
}

fn passes_filter(filter: BlockFilter, block: &FinishedBlock) -> bool {
    match filter {
        BlockFilter::None => true,
        BlockFilter::Failed => block.exit_code != 0,
        BlockFilter::Slow => block.duration_ms >= SLOW_THRESHOLD_MS,
    }
}

/// Set the active filter and toggle each finished block's visibility.
fn apply_filter(ctx: &Rc<Ctx>, filter: BlockFilter) {
    ctx.filter.set(filter);
    select_block(ctx, None);
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(passes_filter(filter, block));
    }
    update_breadcrumb(ctx, ctx.scroll.vadjustment().value());
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
    if let Some(idx) = target {
        scroll_to_block(ctx, idx);
    }
}

/// Step the block selection by `delta` (+1 = next/down, -1 = prev/up) over the
/// currently *visible* finished blocks, clamping at the ends. With no current
/// selection, Up selects the last visible block and Down selects the first.
fn step_block_selection(ctx: &Rc<Ctx>, delta: i32) {
    let visible: Vec<usize> = ctx
        .finished
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, b)| b.widget.is_visible())
        .map(|(i, _)| i)
        .collect();
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
    ctx.breadcrumb.set_visible(false);
    ctx.breadcrumb_target.set(None);
    ctx.search_matches.borrow_mut().clear();
    ctx.search_idx.set(0);
    ctx.pty.write_bytes(b"\x0c");
}

/// Refresh the floating breadcrumb to name the last visible block whose top has
/// scrolled above the viewport.
fn update_breadcrumb(ctx: &Rc<Ctx>, scroll_top: f64) {
    if ctx.fullscreen.get() {
        return;
    }
    if scroll_top <= 8.0 {
        ctx.breadcrumb.set_visible(false);
        ctx.breadcrumb_target.set(None);
        return;
    }
    let finished = ctx.finished.borrow();
    let mut found: Option<usize> = None;
    for (idx, block) in finished.iter().enumerate().rev() {
        if !block.widget.is_visible() {
            continue;
        }
        if let Some(p) = block
            .widget
            .compute_point(&ctx.scroll, &gtk::graphene::Point::new(0.0, 0.0))
        {
            if p.y() as f64 <= 0.0 {
                found = Some(idx);
                break;
            }
        }
    }
    match found {
        Some(idx) => {
            let cmd = finished[idx].command.lines().next().unwrap_or("").to_string();
            ctx.breadcrumb.set_label(&format!("\u{276f}  {cmd}"));
            ctx.breadcrumb_target.set(Some(idx));
            ctx.breadcrumb.set_visible(true);
        }
        None => {
            ctx.breadcrumb.set_visible(false);
            ctx.breadcrumb_target.set(None);
        }
    }
}

fn reset_active(ctx: &Rc<Ctx>) {
    ctx.active_vte.reset(true, true);
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
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
    duration: Option<Duration>,
    end_time_ms: Option<u64>,
) -> gtk::Box {
    let outer = gtk::Box::new(Orientation::Vertical, 0);
    outer.add_css_class("block-finished");
    if exit_code == 0 {
        outer.add_css_class("block-success");
    } else {
        outer.add_css_class("block-failed");
    }
    outer.set_hexpand(true);

    // Parse ANSI output into styled runs once; `plain_output` is the de-styled
    // text used for the empty check and clipboard copy.
    let palette = ctx.config.borrow().palette;
    let runs = ansi::ansi_text_runs(output, &palette);
    let plain_output: String = runs.iter().map(|r| r.text.as_str()).collect();

    // Header row.
    let header = gtk::Box::new(Orientation::Horizontal, 6);
    header.add_css_class("block-header");

    // Status icon: Nerd Font check () on success, times () on failure.
    let status = gtk::Label::new(Some(if exit_code == 0 { "\u{f00c}" } else { "\u{f00d}" }));
    status.add_css_class(if exit_code == 0 {
        "block-status-ok"
    } else {
        "block-status-bad"
    });
    header.append(&status);

    let cwd_label = gtk::Label::new(Some(&shorten_path(cwd)));
    cwd_label.add_css_class("block-header-label");
    cwd_label.set_xalign(0.0);
    header.append(&cwd_label);

    let spacer = gtk::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);

    if let Some(et) = end_time_ms {
        let ts = gtk::Label::new(Some(&format_clock(et)));
        ts.add_css_class("block-header-label");
        header.append(&ts);
    }

    if let Some(d) = duration {
        let badge = gtk::Label::new(Some(&format_duration(d)));
        badge.add_css_class("block-meta-badge");
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

    // Output view (with collapse-on-overflow), ANSI-colored. Handles are kept so
    // the header chevron can toggle the whole output area's visibility.
    let mut output_view: Option<gtk::TextView> = None;
    let mut show_more: Option<gtk::Button> = None;
    if !plain_output.is_empty() {
        let max_lines = ctx.config.borrow().max_collapsed_output_lines as usize;
        let total_lines = ansi::count_lines(&runs);
        if max_lines > 0 && total_lines > max_lines {
            let head_chars = ansi::char_offset_after_lines(&runs, max_lines);
            let head_runs = ansi::truncate_runs(&runs, head_chars);
            let view = ansi_output_view(&head_runs, "block-output-view");
            outer.append(&view);

            let hidden = total_lines - max_lines;
            let btn = gtk::Button::with_label(&format!("▼ show {hidden} more lines"));
            btn.add_css_class("block-show-more");
            btn.set_halign(gtk::Align::Start);
            {
                let full = runs.clone();
                let view = view.clone();
                btn.connect_clicked(move |btn| {
                    render_ansi_runs(&view, &full);
                    btn.set_visible(false);
                });
            }
            outer.append(&btn);
            output_view = Some(view);
            show_more = Some(btn);
        } else {
            let view = ansi_output_view(&runs, "block-output-view");
            outer.append(&view);
            output_view = Some(view);
        }
    }

    // Wire the collapse chevron to toggle the output area. Blocks with no output
    // start collapsed (chevron pointing right), matching jterm4.
    let has_output = output_view.is_some();
    {
        let output_view = output_view.clone();
        let show_more = show_more.clone();
        collapse_btn.connect_clicked(move |btn| {
            let collapsed = btn.label().map(|l| l == "\u{f054}").unwrap_or(false);
            let show = collapsed;
            if let Some(v) = &output_view {
                v.set_visible(show);
            }
            if let Some(b) = &show_more {
                b.set_visible(show);
            }
            btn.set_label(if show { "\u{f078}" } else { "\u{f054}" });
        });
    }
    if !has_output {
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

    outer
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

    let items: [(&str, fn(&Rc<Ctx>, u64)); 4] = [
        ("Copy Block", copy_block_by_id),
        ("Export as JSON", export_block_json),
        ("Export as Markdown", export_block_markdown),
        ("Delete Block", delete_block),
    ];
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
/// selection, breadcrumb) are reset since deletion shifts positions.
fn delete_block(ctx: &Rc<Ctx>, id: u64) {
    let mut finished = ctx.finished.borrow_mut();
    if let Some(pos) = finished.iter().position(|b| b.id == id) {
        let block = finished.remove(pos);
        ctx.block_list.remove(&block.widget);
    }
    drop(finished);
    ctx.selected_block.set(None);
    ctx.search_matches.borrow_mut().clear();
    ctx.search_idx.set(0);
    update_breadcrumb(ctx, ctx.scroll.vadjustment().value());
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
        .block-breadcrumb {{
            color: {dim_fg};
            background-color: {block_bg_hex};
            border: 1px solid rgba({acc_r},{acc_g},{acc_b},0.28);
            border-left: 3px solid {accent};
            border-radius: 999px;
            margin: 8px 14px;
            padding: 4px 14px;
            font-family: "{font_family}";
            font-size: 0.85em;
            box-shadow: 0 4px 12px rgba(0,0,0,0.32);
        }}
        .block-breadcrumb:hover {{
            color: {fg_hex};
            background-color: rgba({acc_r},{acc_g},{acc_b},0.10);
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
