//! VTE terminal backend as a relm4 Component.
//!
//! Wraps a `vte4::Terminal` + `gtk4::Scrollbar` in a horizontal box. The shell
//! is spawned on init. VTE signals (cwd/exit/bell/title/activity) are forwarded
//! as component Output messages instead of jterm4's callback-Vec observer model.

use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::gio::{self, Cancellable};
use gtk4::glib::translate::IntoGlib;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::Orientation;
use gtk4::GestureClick;
use relm4::gtk;
use relm4::prelude::*;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;

// ─── Terminal widget construction (ported from jterm4 terminal.rs) ──────────

pub(crate) fn create_terminal(config: &Config) -> Terminal {
    let font_scale = config.default_font_scale;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);
    // Backspace must send DEL (0x7f), not BS (0x08); readline/most shells only
    // erase on DEL, so the default binding leaves Backspace unable to delete.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    terminal.set_delete_binding(vte4::EraseBinding::DeleteSequence);

    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));

    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));

    if let Ok(regex_pattern) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex_pattern, 0);
    }

    terminal
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk::Box {
    let hbox = gtk::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    hbox.append(terminal);
    hbox.append(&scrollbar);
    hbox
}

pub(crate) fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    if let Some(uri) = terminal.current_directory_uri() {
        let file = gio::File::for_uri(uri.as_str());
        if let Some(path) = file
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(path);
        }
    }
    let pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

pub(crate) fn default_tab_title(tab_index_1based: u32, working_directory: Option<&str>) -> String {
    let mut resolved_dir = working_directory
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    if resolved_dir.is_none() {
        resolved_dir = std::env::var("HOME").ok();
    }

    let Some(dir) = resolved_dir.as_deref() else {
        return format!("Terminal {tab_index_1based}");
    };

    let mut normalized = dir.trim_end_matches('/');
    if normalized.is_empty() {
        normalized = "/";
    }

    let home = std::env::var("HOME").ok();
    let display_dir = if let Some(home) = home.as_deref() {
        if normalized == home {
            "~".to_string()
        } else if let Some(rest) = normalized.strip_prefix(home) {
            if rest.starts_with('/') {
                format!("~{rest}")
            } else {
                normalized.to_string()
            }
        } else {
            normalized.to_string()
        }
    } else {
        normalized.to_string()
    };

    if display_dir == "/" || display_dir == "~" {
        return display_dir;
    }

    fn shorten_component(component: &str) -> String {
        if component.is_empty() {
            return String::new();
        }
        if component == "." || component == ".." {
            return component.to_string();
        }
        let mut chars = component.chars();
        let first = chars.next().unwrap();
        if first == '.' {
            if let Some(second) = chars.next() {
                let mut out = String::new();
                out.push(first);
                out.push(second);
                out
            } else {
                ".".to_string()
            }
        } else {
            first.to_string()
        }
    }

    let (prefix, rest) = if let Some(r) = display_dir.strip_prefix("~/") {
        ("~/", r)
    } else if let Some(r) = display_dir.strip_prefix('/') {
        ("/", r)
    } else {
        ("", display_dir.as_str())
    };

    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 1 {
        return format!("{prefix}{rest}");
    }

    let mut out_parts: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            out_parts.push((*part).to_string());
        } else {
            out_parts.push(shorten_component(part));
        }
    }

    format!("{prefix}{}", out_parts.join("/"))
}

pub(crate) fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        log::warn!("Failed to open URI {uri}: {err}");
    }
}

/// Ctrl+Click on a hyperlink opens it; other clicks pass through to VTE selection.
pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    let click_controller = GestureClick::new();
    click_controller.set_button(GDK_BUTTON_PRIMARY as u32);
    click_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let terminal_clone = terminal.clone();
    click_controller.connect_pressed(move |controller, n_press, x, y| {
        if n_press == 1 {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                if let Some(uri) = terminal_clone.check_match_at(x, y).0 {
                    open_uri(&uri);
                    controller.set_state(gtk::EventSequenceState::Claimed);
                    return;
                }
            }
        }
        controller.set_state(gtk::EventSequenceState::Denied);
    });
    terminal.add_controller(click_controller);
}

pub(crate) fn spawn_shell(
    terminal: &Terminal,
    argv_owned: &[String],
    working_directory: Option<&str>,
    session_id: Option<&str>,
    initial_commands: Option<&str>,
    probe: PaneProbe,
) {
    let mut argv_vec: Vec<String> = argv_owned.to_vec();
    if let Some(sid) = session_id {
        let is_rsh = argv_vec
            .first()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            .map(|name| name == "rsh")
            .unwrap_or(false);
        if is_rsh {
            argv_vec.push("--session".to_string());
            argv_vec.push(sid.to_string());
        }
    }
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // Advertise ourselves so users can gate `source jterm1.{bash,zsh,fish}` on
    // `[[ $TERM_PROGRAM == jterm1 ]]` in their rc files.
    let envv: &[&str] = &["TERM_PROGRAM=jterm1"];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let home = std::env::var("HOME").ok();
    let working_directory = working_directory.or(home.as_deref());
    let terminal_for_pid = terminal.clone();

    let init_cmds = initial_commands.map(|s| s.to_string());
    let terminal_for_init = terminal.clone();

    terminal.spawn_async(
        PtyFlags::DEFAULT,
        working_directory,
        &argv,
        envv,
        spawn_flags,
        || {},
        -1,
        cancellable,
        move |res| {
            log::debug!("spawn_async: {res:?}");
            if let Ok(pid) = res {
                let pid_i32: i32 = pid.into_glib();
                unsafe {
                    terminal_for_pid.set_data::<i32>("child-pid", pid_i32);
                }
                probe.shell_pid.set(pid_i32);
                if let Some(pty) = terminal_for_pid.pty() {
                    use std::os::fd::AsRawFd;
                    probe.pty_fd.set(pty.fd().as_raw_fd());
                }
            }
            if let Some(ref cmds) = init_cmds {
                if !cmds.is_empty() {
                    let cmds = cmds.clone();
                    gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(500),
                        move || {
                            let lines: Vec<&str> = cmds.split(", ").collect();
                            for line in lines {
                                let text = format!("{}\r", line.trim());
                                terminal_for_init.feed_child(text.as_bytes());
                            }
                        },
                    );
                }
            }
        },
    );
}

// ─── VteTerminal relm4 Component ────────────────────────────────────────────

pub struct VteInit {
    pub config: Rc<RefCell<Config>>,
    pub shell_argv: Rc<Vec<String>>,
    pub working_directory: Option<String>,
    pub session_id: Option<String>,
    pub initial_commands: Option<String>,
    pub probe: PaneProbe,
}

/// Shared, cheaply-clonable handle exposing a pane's shell pid and PTY master fd
/// to the app, so it can probe the foreground process (for restorable-command
/// detection and close-confirmation) without a synchronous round-trip into the
/// backend component. Both fields default to -1/0 until the shell is spawned.
#[derive(Clone, Default)]
pub struct PaneProbe {
    pub shell_pid: Rc<Cell<i32>>,
    pub pty_fd: Rc<Cell<i32>>,
}

#[derive(Debug)]
pub enum VteInput {
    WriteInput(Vec<u8>),
    Resize(u16, u16),
    GrabFocus,
    Copy,
    /// Block-view only: when a finished block is selected, copy its output
    /// only (Warp's Alt+Ctrl+Shift+C). Falls back to a regular Copy elsewhere.
    CopyOutputOnly,
    Paste,
    SetFontScale(f64),
    SetFont(String),
    SetScrollback(i64),
    ScrollLines(i32),
    ApplyTheme,
    Kill,
    /// Block-view only: show only failed / only slow / only pinned / all blocks.
    FilterFailedBlocks,
    FilterSlowBlocks,
    FilterPinnedBlocks,
    ClearBlockFilter,
    /// Block-view only: jump to the previous / next pinned block.
    JumpToPrevPinned,
    JumpToNextPinned,
    /// Search: set the query and jump to the first match. `use_regex` treats the
    /// query as a regex; otherwise it is matched literally (case-insensitive).
    SearchSet(String, bool),
    SearchNext,
    SearchPrev,
    SearchClear,
}

#[derive(Debug)]
pub enum VteOutput {
    CwdChanged(String),
    Exited(i32),
    Bell,
    TitleChanged(String),
    Activity,
    Focused,
    /// A command finished while the user wasn't looking (tab inactive or
    /// scrolled away from the bottom). `true` = success, `false` = failure.
    /// Only emitted by BlockTerminal.
    CommandFinished(bool),
}

pub struct VteTerminal {
    terminal: Terminal,
    config: Rc<RefCell<Config>>,
}

impl VteTerminal {
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }
}

impl Component for VteTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk::Box;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Box::new(Orientation::Horizontal, 0)
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let terminal = create_terminal(&init.config.borrow());

        // Build the scrollbar wrapper directly into the provided root box.
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("terminal-box");
        let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
        root.append(&terminal);
        root.append(&scrollbar);

        setup_terminal_click_handler(&terminal);

        // Forward VTE signals as Output messages.
        {
            let sender = sender.clone();
            let term_for_cwd = terminal.clone();
            terminal.connect_current_directory_uri_notify(move |_| {
                if let Some(uri) = term_for_cwd.current_directory_uri() {
                    let file = gio::File::for_uri(uri.as_str());
                    if let Some(path) = file
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                    {
                        let _ = sender.output(VteOutput::CwdChanged(path));
                    }
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_child_exited(move |_term, status| {
                let _ = sender.output(VteOutput::Exited(status));
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_bell(move |_term| {
                let _ = sender.output(VteOutput::Bell);
            });
        }
        {
            let sender = sender.clone();
            let term_for_title = terminal.clone();
            terminal.connect_window_title_changed(move |_term| {
                if let Some(title) = term_for_title.window_title() {
                    let title_str = title.to_string();
                    if !title_str.is_empty() {
                        let _ = sender.output(VteOutput::TitleChanged(title_str));
                    }
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_contents_changed(move |_term| {
                let _ = sender.output(VteOutput::Activity);
            });
        }

        spawn_shell(
            &terminal,
            &init.shell_argv,
            init.working_directory.as_deref(),
            init.session_id.as_deref(),
            init.initial_commands.as_deref(),
            init.probe.clone(),
        );

        // Grab focus once the widget is realized.
        {
            let term_for_focus = terminal.clone();
            terminal.connect_realize(move |_| {
                term_for_focus.grab_focus();
            });
        }

        // Report focus-enter so the app can track the active pane.
        {
            let sender = sender.clone();
            let focus_ctl = gtk::EventControllerFocus::new();
            focus_ctl.connect_enter(move |_| {
                let _ = sender.output(VteOutput::Focused);
            });
            terminal.add_controller(focus_ctl);
        }

        let model = VteTerminal {
            terminal,
            config: init.config,
        };
        ComponentParts {
            model,
            widgets: (),
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            VteInput::WriteInput(data) => self.terminal.feed_child(&data),
            VteInput::Resize(cols, rows) => {
                if let Some(pty) = self.terminal.pty() {
                    let _ = pty.set_size(rows as i32, cols as i32);
                }
            }
            VteInput::GrabFocus => {
                self.terminal.grab_focus();
            }
            VteInput::Copy | VteInput::CopyOutputOnly => {
                self.terminal.copy_clipboard_format(vte4::Format::Text)
            }
            VteInput::Paste => self.terminal.paste_clipboard(),
            VteInput::SetFontScale(scale) => self.terminal.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let fd = FontDescription::from_string(&desc);
                self.terminal.set_font(Some(&fd));
            }
            VteInput::SetScrollback(lines) => self.terminal.set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => {
                if let Some(adj) = self.terminal.vadjustment() {
                    let delta = adj.step_increment() * lines as f64;
                    let max_val = adj.upper() - adj.page_size();
                    let new_val = (adj.value() + delta).clamp(adj.lower(), max_val.max(adj.lower()));
                    adj.set_value(new_val);
                }
            }
            VteInput::ApplyTheme => {
                let config = self.config.borrow();
                let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
                self.terminal.set_colors(
                    Some(&config.foreground),
                    Some(&config.background),
                    &palette_refs,
                );
                self.terminal.set_color_bold(None);
                self.terminal.set_color_cursor(Some(&config.cursor));
                self.terminal
                    .set_color_cursor_foreground(Some(&config.cursor_foreground));
            }
            VteInput::Kill => {
                if let Some(pid) = unsafe { self.terminal.data::<i32>("child-pid") } {
                    let pid_val = unsafe { *pid.as_ref() };
                    unsafe {
                        nix::libc::kill(pid_val, nix::libc::SIGHUP);
                    }
                }
            }
            // Block-view only; no-op for the bare VTE backend.
            VteInput::FilterFailedBlocks
            | VteInput::FilterSlowBlocks
            | VteInput::FilterPinnedBlocks
            | VteInput::ClearBlockFilter
            | VteInput::JumpToPrevPinned
            | VteInput::JumpToNextPinned => {}
            VteInput::SearchSet(query, use_regex) => {
                let pattern = if use_regex {
                    query
                } else {
                    gtk4::glib::Regex::escape_string(&query).to_string()
                };
                if let Ok(regex) = vte4::Regex::for_search(&pattern, pcre2_sys::PCRE2_CASELESS) {
                    self.terminal.search_set_regex(Some(&regex), 0);
                    self.terminal.search_set_wrap_around(true);
                    self.terminal.search_find_next();
                }
            }
            VteInput::SearchNext => {
                self.terminal.search_find_next();
            }
            VteInput::SearchPrev => {
                self.terminal.search_find_previous();
            }
            VteInput::SearchClear => {
                self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
            }
        }
    }
}
