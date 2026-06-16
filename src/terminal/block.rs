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
use std::time::{Duration, Instant};
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

/// Metadata + widget for one finished block, used by filtering and breadcrumb.
struct FinishedBlock {
    widget: gtk::Box,
    command: String,
    /// De-styled output text, kept for full-text search.
    plain_output: String,
    exit_code: i32,
    duration_ms: u64,
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
    out_buf: RefCell<Vec<u8>>,
    exit_code: Cell<i32>,
    cwd: RefCell<String>,
    start_time: Cell<Option<Instant>>,
    duration: Cell<Option<Duration>>,
    has_command: Cell<bool>,
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
            out_buf: RefCell::new(Vec::new()),
            exit_code: Cell::new(0),
            cwd: RefCell::new(init.working_directory.clone().unwrap_or_default()),
            start_time: Cell::new(None),
            duration: Cell::new(None),
            has_command: Cell::new(false),
            finished: RefCell::new(Vec::new()),
            filter: Cell::new(BlockFilter::None),
            breadcrumb: breadcrumb.clone(),
            breadcrumb_target: Cell::new(None),
            search_matches: RefCell::new(Vec::new()),
            search_idx: Cell::new(0),
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
                BlockState::AwaitingCommand => ctx.cmd_buf.borrow_mut().extend_from_slice(&bytes),
                BlockState::CollectingOutput => {
                    ctx.out_buf.borrow_mut().extend_from_slice(&bytes);
                    let _ = sender.output(VteOutput::Activity);
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
            ctx.state.set(BlockState::PostCommand);
        }
        ParserEvent::CwdUpdate(path) => {
            *ctx.cwd.borrow_mut() = path.clone();
            let _ = sender.output(VteOutput::CwdChanged(path));
        }
        ParserEvent::AltScreenEnter => {
            ctx.prev_state.set(ctx.state.get());
            ctx.state.set(BlockState::AltScreen);
            ctx.active_vte.feed(b"\x1b[?1049h");
        }
        ParserEvent::AltScreenLeave => {
            ctx.active_vte.feed(b"\x1b[?1049l");
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
    let adj = ctx.scroll.vadjustment();
    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
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
    let duration = ctx.duration.get();

    let block = build_finished_block(ctx, &command, &output, exit_code, &cwd, duration);
    ctx.block_list.append(&block);
    ctx.block_list
        .reorder_child_after(&ctx.active_holder, Some(&block));

    let duration_ms = duration.map(|d| d.as_millis() as u64).unwrap_or(0);
    let meta = FinishedBlock {
        widget: block.clone(),
        command: command.clone(),
        plain_output: strip_ansi(output.as_bytes()),
        exit_code,
        duration_ms,
    };
    block.set_visible(passes_filter(ctx.filter.get(), &meta));
    ctx.finished.borrow_mut().push(meta);

    reset_active(ctx);
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
    for block in ctx.finished.borrow().iter() {
        block.widget.set_visible(passes_filter(filter, block));
    }
    update_breadcrumb(ctx, ctx.scroll.vadjustment().value());
}

/// Refresh the floating breadcrumb to name the last visible block whose top has
/// scrolled above the viewport.
fn update_breadcrumb(ctx: &Rc<Ctx>, scroll_top: f64) {
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
    ctx.start_time.set(None);
}

// ─── Finished-block widget ──────────────────────────────────────────────────

fn build_finished_block(
    ctx: &Rc<Ctx>,
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
    duration: Option<Duration>,
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

    let status = gtk::Label::new(Some(if exit_code == 0 { "✓" } else { "✗" }));
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
    // is hovered.
    let action_box = gtk::Box::new(Orientation::Horizontal, 2);
    action_box.set_visible(false);

    let copy_cmd = gtk::Button::with_label("⧉");
    copy_cmd.add_css_class("block-action-btn");
    copy_cmd.set_tooltip_text(Some("Copy command"));
    {
        let cmd = command.to_string();
        copy_cmd.connect_clicked(move |_| set_clipboard(&cmd));
    }
    action_box.append(&copy_cmd);

    let copy_out = gtk::Button::with_label("⎘");
    copy_out.add_css_class("block-action-btn");
    copy_out.set_tooltip_text(Some("Copy output"));
    {
        let out = plain_output.clone();
        copy_out.connect_clicked(move |_| set_clipboard(&out));
    }
    action_box.append(&copy_out);

    let rerun = gtk::Button::with_label("↻");
    rerun.add_css_class("block-action-btn");
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

    // Output view (with collapse-on-overflow), ANSI-colored.
    if !plain_output.is_empty() {
        let max_lines = ctx.config.borrow().max_collapsed_output_lines as usize;
        let total_lines = ansi::count_lines(&runs);
        if max_lines > 0 && total_lines > max_lines {
            let head_chars = ansi::char_offset_after_lines(&runs, max_lines);
            let head_runs = ansi::truncate_runs(&runs, head_chars);
            let output_view = ansi_output_view(&head_runs, "block-output-view");
            outer.append(&output_view);

            let hidden = total_lines - max_lines;
            let show_more = gtk::Button::with_label(&format!("▼ show {hidden} more lines"));
            show_more.add_css_class("block-show-more");
            show_more.set_halign(gtk::Align::Start);
            {
                let full = runs.clone();
                let view = output_view.clone();
                show_more.connect_clicked(move |btn| {
                    render_ansi_runs(&view, &full);
                    btn.set_visible(false);
                });
            }
            outer.append(&show_more);
        } else {
            let output_view = ansi_output_view(&runs, "block-output-view");
            outer.append(&output_view);
        }
    }

    outer
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
            if b == 0x07 {
                in_osc = false;
            } else if prev_esc && b == b'\\' {
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
