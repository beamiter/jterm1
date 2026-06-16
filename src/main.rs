#![allow(dead_code)]

mod config;
mod dialogs;
mod file_tree;
mod keybindings;
mod parser;
mod process;
mod pty;
mod session;
mod terminal;

use relm4::prelude::*;
use relm4::adw;
use relm4::gtk;
use adw::prelude::*;
use gtk::gdk::ModifierType;
use gtk::gio::{self, Cancellable};
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use config::{choose_shell_argv, config_file_path, load_config, Config, TerminalMode, Theme};
use keybindings::{normalize_key, Action, Direction, KeyCombo, KeybindingMap};
use terminal::{default_tab_title, BlockTerminal, VteInit, VteInput, VteOutput, VteTerminal};

const FONT_STEP: f64 = 0.025;
const OPACITY_STEP: f64 = 0.025;

#[derive(Debug, Clone)]
enum AppMsg {
    NewTab,
    CloseTab(u64),
    /// Close without the running-process confirmation (dialog already approved).
    ForceCloseTab(u64),
    ForceClosePane(u64),
    SelectTab(u64),
    NextTab,
    PrevTab,
    ToggleSidebar,
    Action(Action),
    ReloadConfig,
    PaneExited(u64, u64),
    PaneCwdChanged(u64, u64, String),
    PaneFocused(u64, u64),
    TitleChanged(u64, String),
    Bell(u64),
    Activity(u64),
    // Settings dialog edits (applied to live config + persisted).
    SettingsTheme(usize),
    SettingsFontDesc(String),
    SettingsFontScale(f64),
    SettingsOpacity(f64),
    SettingsScrollback(u32),
    // Search bar.
    SearchChanged(String),
    SearchNext,
    SearchPrev,
    SearchClose,
    // Tab management.
    RenameTab(u64, String),
    /// Drag-and-drop reorder: move the tab with this id to the target index.
    ReorderTab(u64, usize),
    SetTabFilter(String),
    /// File tree: insert a file's shell-quoted path into the active terminal.
    FileTreeActivateFile(String),
    /// File tree: reroot to the active tab's working directory.
    FileTreeGotoCwd,
    /// File tree: move the root up to its parent directory.
    FileTreeGoUp,
    Ignore,
}

/// Holds either backend; both share `VteInput`/`VteOutput` so callers stay
/// backend-agnostic.
enum TermCtl {
    Vte(Controller<VteTerminal>),
    Block(Controller<BlockTerminal>),
}

impl TermCtl {
    fn emit(&self, msg: VteInput) {
        match self {
            TermCtl::Vte(c) => c.emit(msg),
            TermCtl::Block(c) => c.emit(msg),
        }
    }

    fn widget(&self) -> gtk::Widget {
        match self {
            TermCtl::Vte(c) => c.widget().clone().upcast(),
            TermCtl::Block(c) => c.widget().clone().upcast(),
        }
    }
}

struct Pane {
    terminal: TermCtl,
    id: u64,
    cwd: Option<String>,
    mode: TerminalMode,
    probe: terminal::PaneProbe,
}

impl Pane {
    /// A restorable command running in this pane (ssh/nix develop/docker exec/…),
    /// or None if just the shell is foreground.
    fn restorable_command(&self) -> Option<String> {
        process::restorable_command(self.probe.pty_fd.get(), self.probe.shell_pid.get())
    }

    /// Name of the foreground process for the close-confirmation prompt.
    fn foreground_process(&self) -> Option<String> {
        process::foreground_process_name(self.probe.pty_fd.get(), self.probe.shell_pid.get())
    }
}

/// Saved tree position of the active pane while a tab is pane-zoomed.
struct ZoomState {
    tree_root: gtk::Widget,
    pane_widget: gtk::Widget,
    parent: gtk::Paned,
    was_start: bool,
}

struct Tab {
    holder: gtk::Box,
    panes: Vec<Pane>,
    active_pane: usize,
    title: String,
    custom_title: bool,
    bell: bool,
    activity: bool,
    marked: bool,
    id: u64,
    zoom: Option<ZoomState>,
}

struct AppModel {
    config: Rc<RefCell<Config>>,
    themes: Rc<Vec<Theme>>,
    kbmap: Rc<RefCell<KeybindingMap>>,
    shell_argv: Rc<Vec<String>>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    next_pane_id: u64,
    sidebar_visible: bool,
    font_scale: f64,
    window_opacity: f64,
    stack: gtk::Stack,
    tab_strip: gtk::Box,
    window: adw::ApplicationWindow,
    dyn_css: gtk::CssProvider,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    tab_filter_entry: gtk::SearchEntry,
    tab_filter: String,
    file_tree_store: gtk::TreeStore,
    file_tree_root_label: gtk::Label,
    file_tree_root: Rc<RefCell<std::path::PathBuf>>,
    command_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    settings_dialog: Rc<RefCell<Option<adw::PreferencesDialog>>>,
    debug_dashboard_dialog: Rc<RefCell<Option<adw::Dialog>>>,
}

#[allow(clippy::too_many_arguments)]
fn create_pane(
    config: &Rc<RefCell<Config>>,
    shell_argv: &Rc<Vec<String>>,
    tab_id: u64,
    pane_id: u64,
    mode: TerminalMode,
    initial_commands: Option<String>,
    working_directory: Option<String>,
    sender: &ComponentSender<AppModel>,
) -> Pane {
    let probe = terminal::PaneProbe::default();
    // -1 means "no PTY yet"; foreground probing skips it (0 would alias stdin).
    probe.pty_fd.set(-1);
    let init = VteInit {
        config: config.clone(),
        shell_argv: shell_argv.clone(),
        working_directory: working_directory.clone(),
        session_id: None,
        initial_commands,
        probe: probe.clone(),
    };
    let forward = move |out| match out {
        VteOutput::Exited(_) => AppMsg::PaneExited(tab_id, pane_id),
        VteOutput::CwdChanged(p) => AppMsg::PaneCwdChanged(tab_id, pane_id, p),
        VteOutput::TitleChanged(t) => AppMsg::TitleChanged(tab_id, t),
        VteOutput::Bell => AppMsg::Bell(tab_id),
        VteOutput::Activity => AppMsg::Activity(tab_id),
        VteOutput::Focused => AppMsg::PaneFocused(tab_id, pane_id),
    };
    let terminal = match mode {
        TerminalMode::Block => TermCtl::Block(
            BlockTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward),
        ),
        TerminalMode::Vte => TermCtl::Vte(
            VteTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward),
        ),
    };
    Pane {
        terminal,
        id: pane_id,
        cwd: working_directory,
        mode,
        probe,
    }
}

impl AppModel {
    fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn active_terminal(&self) -> Option<&TermCtl> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .map(|p| &p.terminal)
    }

    /// Working directory of the active pane, if it reports one.
    fn active_cwd(&self) -> Option<std::path::PathBuf> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .and_then(|p| p.cwd.clone())
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
    }

    /// Rebuild the file tree with `root` at the top.
    fn set_file_tree_root(&self, root: std::path::PathBuf) {
        self.file_tree_store.clear();
        self.file_tree_root_label.set_text(&file_tree::display_path(&root));
        self.file_tree_root_label
            .set_tooltip_text(Some(&root.to_string_lossy()));
        file_tree::populate_dir(&self.file_tree_store, None, &root);
        *self.file_tree_root.borrow_mut() = root;
    }

    /// Initialize the file tree to the active cwd, else `$HOME`, else `/`.
    fn init_file_tree(&self) {
        let start = self
            .active_cwd()
            .or_else(file_tree::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.set_file_tree_root(start);
    }

    /// Jump the file tree to the active tab's working directory.
    fn file_tree_goto_current_cwd(&self) {
        match self.active_cwd() {
            Some(dir) => {
                if *self.file_tree_root.borrow() != dir {
                    self.set_file_tree_root(dir);
                }
            }
            None => {
                if self.file_tree_root.borrow().as_os_str().is_empty() {
                    if let Some(home) = file_tree::home_dir() {
                        self.set_file_tree_root(home);
                    }
                }
            }
        }
    }

    /// Move the file tree root up to its parent directory.
    fn file_tree_go_up(&self) {
        let parent = self
            .file_tree_root
            .borrow()
            .parent()
            .map(std::path::Path::to_path_buf);
        if let Some(parent) = parent {
            self.set_file_tree_root(parent);
        }
    }

    fn add_tab(&mut self, initial_commands: Option<String>, sender: &ComponentSender<AppModel>) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let number = self.tabs.len() as u32 + 1;
        let mode = self.config.borrow().terminal_mode;
        let pane = create_pane(
            &self.config,
            &self.shell_argv,
            id,
            pane_id,
            mode,
            initial_commands,
            None,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: default_tab_title(number, None),
            custom_title: false,
            bell: false,
            activity: false,
            marked: false,
            id,
            zoom: None,
        };
        self.tabs.push(tab);
        self.select_tab(id, sender);
    }

    /// Recreate a tab from a persisted snapshot, rebuilding the full nested
    /// `Paned` split tree and replaying any restorable command per pane.
    fn restore_tab(&mut self, saved: &session::SavedTab, sender: &ComponentSender<AppModel>) {
        let id = self.next_id;
        self.next_id += 1;
        let mut panes = Vec::new();
        let root_widget = self.build_pane_layout(&saved.layout, id, &mut panes, sender);
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&root_widget);
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes,
            active_pane: 0,
            title: saved.title.clone(),
            custom_title: saved.custom_title,
            bell: false,
            activity: false,
            marked: false,
            id,
            zoom: None,
        };
        self.tabs.push(tab);
    }

    /// Recursively build the GTK widget tree for a persisted `PaneLayout`,
    /// pushing each created leaf into `panes` in tree order.
    fn build_pane_layout(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        match node {
            session::PaneLayout::Leaf { mode, cwd, cmds } => {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let pane = create_pane(
                    &self.config,
                    &self.shell_argv,
                    tab_id,
                    pane_id,
                    session::PaneLayout::terminal_mode(mode),
                    cmds.clone(),
                    cwd.clone(),
                    sender,
                );
                let widget = pane.terminal.widget();
                panes.push(pane);
                widget
            }
            session::PaneLayout::Split {
                orientation,
                position,
                start,
                end,
            } => {
                let o = if *orientation == 'v' {
                    gtk::Orientation::Vertical
                } else {
                    gtk::Orientation::Horizontal
                };
                let paned = gtk::Paned::new(o);
                paned.set_hexpand(true);
                paned.set_vexpand(true);
                let start_w = self.build_pane_layout(start, tab_id, panes, sender);
                let end_w = self.build_pane_layout(end, tab_id, panes, sender);
                paned.set_start_child(Some(&start_w));
                paned.set_end_child(Some(&end_w));
                paned.set_position(*position);
                paned.upcast()
            }
        }
    }

    /// Serialize a tab's live `Paned` widget tree into a persistable `PaneLayout`.
    /// When the tab is pane-zoomed the real tree is detached into `ZoomState`, so
    /// we serialize from there and refill the removed pane's slot.
    fn serialize_layout(&self, tab: &Tab) -> session::PaneLayout {
        let root = tab
            .zoom
            .as_ref()
            .map(|z| z.tree_root.clone())
            .or_else(|| tab.holder.first_child());
        match root {
            Some(w) => self.serialize_widget(tab, &w),
            None => session::PaneLayout::Leaf {
                mode: "block".to_string(),
                cwd: None,
                cmds: None,
            },
        }
    }

    fn serialize_widget(&self, tab: &Tab, widget: &gtk::Widget) -> session::PaneLayout {
        if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
            let orientation = match paned.orientation() {
                gtk::Orientation::Vertical => 'v',
                _ => 'h',
            };
            let start = self.resolve_child(tab, paned, paned.start_child(), true);
            let end = self.resolve_child(tab, paned, paned.end_child(), false);
            session::PaneLayout::Split {
                orientation,
                position: paned.position(),
                start: Box::new(start),
                end: Box::new(end),
            }
        } else {
            let pane = tab.panes.iter().find(|p| p.terminal.widget() == *widget);
            let (mode, cwd, cmds) = match pane {
                Some(p) => (
                    match p.mode {
                        TerminalMode::Vte => "vte",
                        TerminalMode::Block => "block",
                    }
                    .to_string(),
                    p.cwd.clone(),
                    p.restorable_command(),
                ),
                None => ("block".to_string(), None, None),
            };
            session::PaneLayout::Leaf { mode, cwd, cmds }
        }
    }

    /// A `Paned` child, substituting the zoomed-out pane when its slot is empty.
    fn resolve_child(
        &self,
        tab: &Tab,
        paned: &gtk::Paned,
        child: Option<gtk::Widget>,
        want_start: bool,
    ) -> session::PaneLayout {
        if let Some(c) = child {
            return self.serialize_widget(tab, &c);
        }
        if let Some(z) = &tab.zoom {
            if &z.parent == paned && z.was_start == want_start {
                return self.serialize_widget(tab, &z.pane_widget);
            }
        }
        session::PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: None,
            cmds: None,
        }
    }

    /// Capture the current tab list as a persistable snapshot, including each
    /// tab's full split layout.
    fn snapshot_session(&self) -> session::SavedSession {
        let tabs = self
            .tabs
            .iter()
            .map(|t| session::SavedTab {
                title: t.title.clone(),
                custom_title: t.custom_title,
                layout: self.serialize_layout(t),
            })
            .collect();
        session::SavedSession {
            active: self.active,
            tabs,
        }
    }

    fn persist_session(&self) {
        session::save_session(&self.snapshot_session());
    }

    /// App-level diagnostics for the debug dashboard. (jterm4 surfaces per-block
    /// stats from the block backend; jterm1 exposes window/session state — block
    /// internals would need a backend round-trip, noted as a parity gap.)
    fn debug_info_snapshot(&self) -> Vec<(String, Vec<(String, String)>)> {
        let cfg = self.config.borrow();
        let total_panes: usize = self.tabs.iter().map(|t| t.panes.len()).sum();
        let active_tab = self.tabs.get(self.active);
        let session = vec![
            ("Tabs".to_string(), self.tabs.len().to_string()),
            ("Total panes".to_string(), total_panes.to_string()),
            (
                "Active tab".to_string(),
                active_tab.map(|t| t.title.clone()).unwrap_or_default(),
            ),
            (
                "Panes in active tab".to_string(),
                active_tab.map(|t| t.panes.len()).unwrap_or(0).to_string(),
            ),
            (
                "Zoomed".to_string(),
                active_tab
                    .map(|t| t.zoom.is_some().to_string())
                    .unwrap_or_else(|| "false".to_string()),
            ),
        ];
        let appearance = vec![
            ("Theme".to_string(), cfg.theme_name.clone()),
            ("Font".to_string(), cfg.font_desc.clone()),
            ("Font scale".to_string(), format!("{:.3}", self.font_scale)),
            (
                "Opacity".to_string(),
                format!("{:.2}", self.window_opacity),
            ),
            (
                "Terminal mode".to_string(),
                match cfg.terminal_mode {
                    TerminalMode::Vte => "vte",
                    TerminalMode::Block => "block",
                }
                .to_string(),
            ),
            (
                "Scrollback".to_string(),
                cfg.terminal_scrollback_lines.to_string(),
            ),
        ];
        let config = vec![
            (
                "Keybindings".to_string(),
                self.kbmap.borrow().bindings.len().to_string(),
            ),
            ("Remote hosts".to_string(), cfg.remote_hosts.len().to_string()),
            (
                "Startup commands".to_string(),
                cfg.startup_commands.clone().unwrap_or_default(),
            ),
        ];
        vec![
            ("Session".to_string(), session),
            ("Appearance".to_string(), appearance),
            ("Config".to_string(), config),
        ]
    }

    /// Open a new tab that connects to a remote host via ssh (always bare VTE).
    fn add_remote_tab(&mut self, host: &config::RemoteHost, sender: &ComponentSender<AppModel>) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let argv = Rc::new(config::build_remote_argv(host));
        let pane = create_pane(
            &self.config,
            &argv,
            id,
            pane_id,
            TerminalMode::Vte,
            None,
            None,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: host.name.clone(),
            custom_title: true,
            bell: false,
            activity: false,
            marked: false,
            id,
            zoom: None,
        };
        self.tabs.push(tab);
        self.select_tab(id, sender);
    }

    fn select_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        self.active = idx;
        self.stack.set_visible_child_name(&id.to_string());
        {
            let tab = &mut self.tabs[idx];
            tab.bell = false;
            tab.activity = false;
        }
        let tab = &self.tabs[idx];
        if let Some(pane) = tab.panes.get(tab.active_pane) {
            pane.terminal.emit(VteInput::GrabFocus);
        }
        self.file_tree_goto_current_cwd();
        self.rebuild_tab_strip(sender);
    }

    fn close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        let tab = self.tabs.remove(idx);
        self.stack.remove(&tab.holder);
        drop(tab);

        if self.tabs.is_empty() {
            relm4::main_application().quit();
            return;
        }
        let new_idx = if idx >= self.tabs.len() { self.tabs.len() - 1 } else { idx };
        let new_id = self.tabs[new_idx].id;
        self.select_tab(new_id, sender);
    }

    /// First restorable command running in any of a tab's panes, if any.
    fn tab_running_command(&self, idx: usize) -> Option<String> {
        self.tabs
            .get(idx)?
            .panes
            .iter()
            .find_map(|p| p.restorable_command())
    }

    /// Close a tab, first confirming if a process is still running in it.
    fn request_close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        if let Some(idx) = self.index_of(id) {
            if let Some(cmd) = self.tab_running_command(idx) {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceCloseTab(id), sender);
                return;
            }
        }
        self.close_tab(id, sender);
    }

    /// Close a pane, first confirming if a process is still running in it.
    fn request_close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        if let Some((ti, pi)) = self.find_pane(pane_id) {
            if let Some(cmd) = self.tabs[ti].panes[pi].restorable_command() {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceClosePane(pane_id), sender);
                return;
            }
        }
        self.close_pane(pane_id, sender);
    }

    /// Move the tab with `src_id` to `to_idx`, preserving which tab is active.
    fn reorder_tab(&mut self, src_id: u64, to_idx: usize, sender: &ComponentSender<AppModel>) {
        let Some(from) = self.index_of(src_id) else { return };
        let to = to_idx.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return;
        }
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(aid) = active_id {
            self.active = self.index_of(aid).unwrap_or(0);
        }
        self.rebuild_tab_strip(sender);
    }

    fn switch_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i32;
        let idx = ((self.active as i32 + delta) % len + len) % len;
        let id = self.tabs[idx as usize].id;
        self.select_tab(id, sender);
    }

    /// Reorder the active tab one slot left (-1) or right (+1) and keep it active.
    fn move_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.len() < 2 {
            return;
        }
        let from = self.active as i32;
        let to = from + delta;
        if to < 0 || to >= self.tabs.len() as i32 {
            return;
        }
        self.tabs.swap(from as usize, to as usize);
        self.active = to as usize;
        self.rebuild_tab_strip(sender);
    }

    /// Open a new tab inheriting the active tab's mode, cwd and (custom) title.
    fn duplicate_active_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(src) = self.tabs.get(self.active) else { return };
        let cwd = src
            .panes
            .get(src.active_pane)
            .and_then(|p| p.cwd.clone());
        let title = src.title.clone();
        let custom_title = src.custom_title;

        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let mode = self.config.borrow().terminal_mode;
        let pane = create_pane(
            &self.config,
            &self.shell_argv,
            id,
            pane_id,
            mode,
            None,
            cwd,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title,
            custom_title,
            bell: false,
            activity: false,
            marked: false,
            id,
            zoom: None,
        };
        self.tabs.push(tab);
        self.select_tab(id, sender);
    }

    /// Close every marked tab (marking is the multi-select model in jterm1).
    fn close_marked_tabs(&mut self, sender: &ComponentSender<AppModel>) {
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.marked)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.close_tab(id, sender);
        }
    }

    fn find_pane(&self, pane_id: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            if let Some(pi) = tab.panes.iter().position(|p| p.id == pane_id) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Split the active pane, placing a fresh bare-VTE pane beside it.
    fn split_active(&mut self, orientation: gtk::Orientation, sender: &ComponentSender<AppModel>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let tab_id = tab.id;
        let api = tab.active_pane;
        let cur_widget = tab.panes[api].terminal.widget();
        let wd = tab.panes[api].cwd.clone();

        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let new_pane = create_pane(
            &self.config,
            &self.shell_argv,
            tab_id,
            pane_id,
            TerminalMode::Vte,
            None,
            wd,
            sender,
        );
        let new_widget = new_pane.terminal.widget();

        let paned = gtk::Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);

        if let Some(parent) = cur_widget.parent() {
            if let Ok(pp) = parent.clone().downcast::<gtk::Paned>() {
                let is_start = pp.start_child().as_ref() == Some(&cur_widget);
                if is_start {
                    pp.set_start_child(None::<&gtk::Widget>);
                } else {
                    pp.set_end_child(None::<&gtk::Widget>);
                }
                paned.set_start_child(Some(&cur_widget));
                paned.set_end_child(Some(&new_widget));
                if is_start {
                    pp.set_start_child(Some(&paned));
                } else {
                    pp.set_end_child(Some(&paned));
                }
            } else {
                let holder = &self.tabs[ti].holder;
                holder.remove(&cur_widget);
                paned.set_start_child(Some(&cur_widget));
                paned.set_end_child(Some(&new_widget));
                holder.append(&paned);
            }
        }

        let tab = &mut self.tabs[ti];
        tab.panes.push(new_pane);
        tab.active_pane = tab.panes.len() - 1;
        tab.panes[tab.active_pane].terminal.emit(VteInput::GrabFocus);
    }

    /// Remove a pane from its tab, collapsing the Paned tree and promoting the
    /// sibling. Closes the whole tab if it was the last pane.
    fn close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        let Some((ti, pi)) = self.find_pane(pane_id) else { return };
        if self.tabs[ti].zoom.is_some() {
            self.toggle_pane_zoom_for(ti);
        }
        if self.tabs[ti].panes.len() == 1 {
            let tab_id = self.tabs[ti].id;
            self.close_tab(tab_id, sender);
            return;
        }
        let eff = self.tabs[ti].panes[pi].terminal.widget();
        if let Some(parent) = eff.parent() {
            if let Ok(paned) = parent.downcast::<gtk::Paned>() {
                let start = paned.start_child();
                let end = paned.end_child();
                let sibling = if start.as_ref() == Some(&eff) { end } else { start };
                paned.set_start_child(None::<&gtk::Widget>);
                paned.set_end_child(None::<&gtk::Widget>);
                if let Some(sibling) = sibling {
                    let paned_w: gtk::Widget = paned.clone().upcast();
                    if let Some(gp) = paned_w.parent() {
                        if let Ok(gpp) = gp.clone().downcast::<gtk::Paned>() {
                            if gpp.start_child().as_ref() == Some(&paned_w) {
                                gpp.set_start_child(Some(&sibling));
                            } else {
                                gpp.set_end_child(Some(&sibling));
                            }
                        } else {
                            let holder = &self.tabs[ti].holder;
                            holder.remove(&paned_w);
                            holder.append(&sibling);
                        }
                    }
                }
            }
        }

        let tab = &mut self.tabs[ti];
        let removed = tab.panes.remove(pi);
        if tab.active_pane >= tab.panes.len() {
            tab.active_pane = tab.panes.len() - 1;
        }
        let ap = tab.active_pane;
        tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        drop(removed);
    }

    fn cycle_pane_focus(&mut self, delta: i32) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let n = tab.panes.len() as i32;
        if n <= 1 {
            return;
        }
        let cur = tab.active_pane as i32;
        let next = ((cur + delta) % n + n) % n;
        tab.active_pane = next as usize;
        tab.panes[tab.active_pane].terminal.emit(VteInput::GrabFocus);
    }

    fn focus_pane_directional(&mut self, direction: Direction) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.panes.len() <= 1 {
            return;
        }
        let holder: gtk::Widget = tab.holder.clone().upcast();
        let api = tab.active_pane;
        let focused_widget = tab.panes[api].terminal.widget();
        let Some(fb) = focused_widget.compute_bounds(&holder) else { return };
        let fcx = fb.x() + fb.width() / 2.0;
        let fcy = fb.y() + fb.height() / 2.0;

        let mut best: Option<(f32, usize)> = None;
        for (i, pane) in tab.panes.iter().enumerate() {
            if i == api {
                continue;
            }
            let w = pane.terminal.widget();
            let Some(b) = w.compute_bounds(&holder) else { continue };
            let cx = b.x() + b.width() / 2.0;
            let cy = b.y() + b.height() / 2.0;
            let dx = cx - fcx;
            let dy = cy - fcy;
            let in_dir = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };
            if !in_dir {
                continue;
            }
            let dist = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };
            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, i));
            }
        }

        if let Some((_, i)) = best {
            let tab = &mut self.tabs[self.active];
            tab.active_pane = i;
            tab.panes[i].terminal.emit(VteInput::GrabFocus);
        }
    }

    fn resize_pane(&mut self, target: gtk::Orientation, delta: i32) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let api = tab.active_pane;
        let mut widget = tab.panes[api].terminal.widget().parent();
        while let Some(cur) = widget {
            if let Ok(paned) = cur.clone().downcast::<gtk::Paned>() {
                if paned.orientation() == target {
                    let new_pos = (paned.position() + delta).max(0);
                    paned.set_position(new_pos);
                    return;
                }
            }
            widget = cur.parent();
        }
    }

    fn toggle_pane_zoom(&mut self) {
        self.toggle_pane_zoom_for(self.active);
    }

    fn toggle_pane_zoom_for(&mut self, ti: usize) {
        let Some(tab) = self.tabs.get_mut(ti) else { return };
        if let Some(z) = tab.zoom.take() {
            tab.holder.remove(&z.pane_widget);
            if z.was_start {
                z.parent.set_start_child(Some(&z.pane_widget));
            } else {
                z.parent.set_end_child(Some(&z.pane_widget));
            }
            tab.holder.append(&z.tree_root);
            let ap = tab.active_pane;
            tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        } else {
            if tab.panes.len() <= 1 {
                return;
            }
            let api = tab.active_pane;
            let pane_widget = tab.panes[api].terminal.widget();
            let Some(parent) = pane_widget.parent() else { return };
            let Ok(parent_paned) = parent.downcast::<gtk::Paned>() else { return };
            let was_start = parent_paned.start_child().as_ref() == Some(&pane_widget);
            let Some(tree_root) = tab.holder.first_child() else { return };
            if was_start {
                parent_paned.set_start_child(None::<&gtk::Widget>);
            } else {
                parent_paned.set_end_child(None::<&gtk::Widget>);
            }
            tab.holder.remove(&tree_root);
            tab.holder.append(&pane_widget);
            tab.zoom = Some(ZoomState {
                tree_root,
                pane_widget: pane_widget.clone(),
                parent: parent_paned,
                was_start,
            });
            tab.panes[api].terminal.emit(VteInput::GrabFocus);
        }
    }

    /// Detach the active pane from a split tab and host it in a brand-new tab.
    fn move_pane_to_new_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.panes.len() <= 1 || tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let pi = tab.active_pane;
        let eff = tab.panes[pi].terminal.widget();

        // Collapse the source tree, promoting the sibling (same as close_pane).
        if let Some(parent) = eff.parent() {
            if let Ok(paned) = parent.downcast::<gtk::Paned>() {
                let start = paned.start_child();
                let end = paned.end_child();
                let sibling = if start.as_ref() == Some(&eff) { end } else { start };
                paned.set_start_child(None::<&gtk::Widget>);
                paned.set_end_child(None::<&gtk::Widget>);
                if let Some(sibling) = sibling {
                    let paned_w: gtk::Widget = paned.clone().upcast();
                    if let Some(gp) = paned_w.parent() {
                        if let Ok(gpp) = gp.clone().downcast::<gtk::Paned>() {
                            if gpp.start_child().as_ref() == Some(&paned_w) {
                                gpp.set_start_child(Some(&sibling));
                            } else {
                                gpp.set_end_child(Some(&sibling));
                            }
                        } else {
                            let holder = &self.tabs[ti].holder;
                            holder.remove(&paned_w);
                            holder.append(&sibling);
                        }
                    }
                }
            }
        }

        let moved = self.tabs[ti].panes.remove(pi);
        {
            let tab = &mut self.tabs[ti];
            if tab.active_pane >= tab.panes.len() {
                tab.active_pane = tab.panes.len() - 1;
            }
        }

        let new_id = self.next_id;
        self.next_id += 1;
        let mw = moved.terminal.widget();
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&mw);
        self.stack.add_named(&holder, Some(&new_id.to_string()));
        let number = self.tabs.len() as u32 + 1;
        let title = default_tab_title(number, moved.cwd.as_deref());
        let new_tab = Tab {
            holder,
            panes: vec![moved],
            active_pane: 0,
            title,
            custom_title: false,
            bell: false,
            activity: false,
            marked: false,
            id: new_id,
            zoom: None,
        };
        self.tabs.push(new_tab);
        self.select_tab(new_id, sender);
    }

    fn set_font_scale_all(&mut self, scale: f64) {
        self.font_scale = scale;
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFontScale(scale));
            }
        }
    }

    fn set_window_opacity(&mut self, opacity: f64) {
        self.window_opacity = opacity;
        self.window.set_opacity(opacity);
    }

    fn toggle_search(&mut self) {
        let opening = !self.search_bar.is_search_mode();
        self.search_bar.set_search_mode(opening);
        if opening {
            self.search_entry.grab_focus();
        } else {
            if let Some(t) = self.active_terminal() {
                t.emit(VteInput::SearchClear);
                t.emit(VteInput::GrabFocus);
            }
        }
    }

    /// Parse the find-bar text: `/pattern/` means regex, anything else literal.
    fn search_query(text: &str) -> (String, bool) {
        if text.starts_with('/') && text.ends_with('/') && text.len() > 2 {
            (text[1..text.len() - 1].to_string(), true)
        } else {
            (text.to_string(), false)
        }
    }

    fn execute_action(&mut self, action: Action, sender: &ComponentSender<AppModel>) {
        match action {
            Action::NewTab => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, sender);
            }
            Action::CloseTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let id = tab.id;
                    self.request_close_tab(id, sender);
                }
            }
            Action::ClosePaneOrTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let tab_id = tab.id;
                    if tab.panes.len() > 1 {
                        let pane_id = tab.panes[tab.active_pane].id;
                        self.request_close_pane(pane_id, sender);
                    } else {
                        self.request_close_tab(tab_id, sender);
                    }
                }
            }
            Action::SplitHorizontal => self.split_active(gtk::Orientation::Horizontal, sender),
            Action::SplitVertical => self.split_active(gtk::Orientation::Vertical, sender),
            Action::CyclePaneFocusForward => self.cycle_pane_focus(1),
            Action::CyclePaneFocusBackward => self.cycle_pane_focus(-1),
            Action::FocusPaneLeft => self.focus_pane_directional(Direction::Left),
            Action::FocusPaneRight => self.focus_pane_directional(Direction::Right),
            Action::FocusPaneUp => self.focus_pane_directional(Direction::Up),
            Action::FocusPaneDown => self.focus_pane_directional(Direction::Down),
            Action::ResizePaneLeft => self.resize_pane(gtk::Orientation::Horizontal, -40),
            Action::ResizePaneRight => self.resize_pane(gtk::Orientation::Horizontal, 40),
            Action::ResizePaneUp => self.resize_pane(gtk::Orientation::Vertical, -40),
            Action::ResizePaneDown => self.resize_pane(gtk::Orientation::Vertical, 40),
            Action::TogglePaneZoom => self.toggle_pane_zoom(),
            Action::MovePaneToNewTab => self.move_pane_to_new_tab(sender),
            Action::Copy => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Copy);
                }
            }
            Action::Paste => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Paste);
                }
            }
            Action::FontIncrease => {
                let s = (self.font_scale + FONT_STEP).min(10.0);
                self.set_font_scale_all(s);
            }
            Action::FontDecrease => {
                let s = (self.font_scale - FONT_STEP).max(0.1);
                self.set_font_scale_all(s);
            }
            Action::OpacityIncrease => {
                let o = (self.window_opacity + OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::OpacityDecrease => {
                let o = (self.window_opacity - OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                self.tab_strip.set_visible(self.sidebar_visible);
            }
            Action::ToggleCommandPalette => {
                dialogs::toggle_command_palette(
                    &self.window,
                    &self.kbmap,
                    &self.command_palette_dialog,
                    sender,
                );
            }
            Action::ToggleSettings => {
                dialogs::toggle_settings(
                    &self.window,
                    &self.config,
                    &self.themes,
                    self.font_scale,
                    self.window_opacity,
                    &self.settings_dialog,
                    sender,
                );
            }
            Action::ToggleSearch => self.toggle_search(),
            Action::MoveTabLeft => self.move_tab(-1, sender),
            Action::MoveTabRight => self.move_tab(1, sender),
            Action::DuplicateTab => self.duplicate_active_tab(sender),
            Action::ToggleTabMarked => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.marked = !tab.marked;
                }
                self.rebuild_tab_strip(sender);
            }
            Action::CloseSelectedTabs => self.close_marked_tabs(sender),
            Action::FilterTabs => {
                if !self.sidebar_visible {
                    self.sidebar_visible = true;
                    self.tab_strip.set_visible(true);
                }
                self.tab_filter_entry.grab_focus();
            }
            Action::PrevTab => self.switch_tab(-1, sender),
            Action::NextTab => self.switch_tab(1, sender),
            Action::ScrollUp => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ScrollLines(-3));
                }
            }
            Action::ScrollDown => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ScrollLines(3));
                }
            }
            Action::FilterFailedBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::FilterFailedBlocks);
                }
            }
            Action::FilterSlowBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::FilterSlowBlocks);
                }
            }
            Action::ClearBlockFilter => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ClearBlockFilter);
                }
            }
            Action::QuickSwitchTab(n) => {
                if !self.tabs.is_empty() {
                    let last = self.tabs.len() - 1;
                    let target = if n == 9 { last } else { (n as usize).min(last) };
                    let id = self.tabs[target].id;
                    self.select_tab(id, sender);
                }
            }
            Action::ShowRemotePicker => {
                dialogs::show_remote_picker(&self.window, &self.config, sender);
            }
            Action::ToggleDebugDashboard => {
                let info = self.debug_info_snapshot();
                dialogs::toggle_debug_dashboard(
                    &self.window,
                    info,
                    &self.debug_dashboard_dialog,
                );
            }
            Action::ConnectRemote(n) => {
                let host = self.config.borrow().remote_hosts.get(n as usize).cloned();
                if let Some(host) = host {
                    self.add_remote_tab(&host, sender);
                }
            }
        }
    }

    fn reload_config(&mut self) {
        let (new_config, themes, new_kb) = load_config();
        let theme = themes
            .iter()
            .find(|t| t.name == new_config.theme_name)
            .or_else(|| themes.first())
            .cloned();

        {
            let mut config = self.config.borrow_mut();
            config.window_opacity = new_config.window_opacity;
            config.terminal_scrollback_lines = new_config.terminal_scrollback_lines;
            config.font_desc = new_config.font_desc.clone();
            config.default_font_scale = new_config.default_font_scale;
            config.startup_commands = new_config.startup_commands.clone();
            if let Some(theme) = &theme {
                config.theme_name = theme.name.clone();
                config.foreground = theme.foreground;
                config.background = theme.background;
                config.cursor = theme.cursor;
                config.cursor_foreground = theme.cursor_foreground;
                config.palette = theme.palette;
            }
        }

        self.set_window_opacity(new_config.window_opacity);
        let font_desc = self.config.borrow().font_desc.clone();
        let scrollback = new_config.terminal_scrollback_lines as i64;
        self.font_scale = new_config.default_font_scale;
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFontScale(new_config.default_font_scale));
                pane.terminal.emit(VteInput::SetFont(font_desc.clone()));
                pane.terminal.emit(VteInput::SetScrollback(scrollback));
                pane.terminal.emit(VteInput::ApplyTheme);
            }
        }

        *self.kbmap.borrow_mut() = new_kb;
        self.themes = Rc::new(themes);
        self.apply_dynamic_css();
        log::info!("Configuration reloaded from disk");
    }

    fn apply_dynamic_css(&self) {
        let config = self.config.borrow();
        let bg = &config.background;
        let fg = &config.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bgg = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fgg = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
        let css = format!(
            ".terminal-box scrollbar {{ background-color: rgb({br},{bgg},{bb}); }}
             .terminal-box scrollbar trough {{ background-color: rgb({br},{bgg},{bb}); }}
             .terminal-box scrollbar slider {{ background-color: rgba({fr},{fgg},{fb},0.4); }}
             .terminal-box scrollbar slider:hover {{ background-color: rgba({fr},{fgg},{fb},0.7); }}
             .top-bar {{ background-color: rgb({br},{bgg},{bb}); color: rgb({fr},{fgg},{fb}); }}
             .top-bar button {{ color: rgb({fr},{fgg},{fb}); }}
             .tab-strip {{ background-color: rgb({br},{bgg},{bb}); }}
             .tab-strip-btn {{ color: rgba({fr},{fgg},{fb},0.6); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fgg},{fb}); }}"
        );
        self.dyn_css.load_from_data(&css);
    }

    fn rebuild_tab_strip(&self, sender: &ComponentSender<AppModel>) {
        while let Some(child) = self.tab_strip.first_child() {
            self.tab_strip.remove(&child);
        }
        let filter = self.tab_filter.to_lowercase();
        for (idx, tab) in self.tabs.iter().enumerate() {
            if !filter.is_empty() && !tab.title.to_lowercase().contains(&filter) {
                continue;
            }
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.add_css_class("tab-row");

            let select_btn = gtk::ToggleButton::with_label(&tab.title);
            select_btn.set_hexpand(true);
            select_btn.set_active(idx == self.active);
            select_btn.add_css_class("tab-strip-btn");
            if tab.bell {
                select_btn.add_css_class("tab-bell");
            }
            if tab.activity {
                select_btn.add_css_class("tab-activity");
            }
            if tab.marked {
                select_btn.add_css_class("tab-marked");
            }
            let id = tab.id;
            let s = sender.clone();
            select_btn.connect_clicked(move |_| s.input(AppMsg::SelectTab(id)));

            // Double-click to rename: a popover with a prefilled entry.
            let rename = gtk::GestureClick::new();
            rename.set_button(gtk::gdk::ffi::GDK_BUTTON_PRIMARY as u32);
            let id_r = tab.id;
            let title_r = tab.title.clone();
            let s_r = sender.clone();
            let btn_r = select_btn.clone();
            rename.connect_pressed(move |_, n_press, _, _| {
                if n_press != 2 {
                    return;
                }
                let popover = gtk::Popover::new();
                popover.set_parent(&btn_r);
                let entry = gtk::Entry::new();
                entry.set_text(&title_r);
                entry.select_region(0, -1);
                popover.set_child(Some(&entry));
                let s_e = s_r.clone();
                let pop = popover.clone();
                entry.connect_activate(move |e| {
                    s_e.input(AppMsg::RenameTab(id_r, e.text().to_string()));
                    pop.popdown();
                });
                popover.connect_closed(|p| p.unparent());
                popover.popup();
                entry.grab_focus();
            });
            select_btn.add_controller(rename);

            // Right-click context menu (parity with jterm4's tab menu). Items
            // dispatch existing AppMsgs; actions that operate on the active tab
            // are preceded by a SelectTab so they target the clicked tab.
            let ctx = gtk::GestureClick::new();
            ctx.set_button(gtk::gdk::ffi::GDK_BUTTON_SECONDARY as u32);
            let id_c = tab.id;
            let marked_c = tab.marked;
            let title_c = tab.title.clone();
            let s_c = sender.clone();
            let btn_c = select_btn.clone();
            ctx.connect_pressed(move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                let popover = gtk::Popover::new();
                popover.set_parent(&btn_c);
                popover.set_has_arrow(false);
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
                vbox.add_css_class("menu");

                let item = |label: &str| {
                    let b = gtk::Button::with_label(label);
                    b.set_has_frame(false);
                    b.add_css_class("flat");
                    if let Some(child) = b.child() {
                        child.set_halign(gtk::Align::Start);
                    }
                    b
                };

                let entries: [(&str, AppMsg); 5] = [
                    ("New Tab", AppMsg::NewTab),
                    ("Duplicate", AppMsg::Action(Action::DuplicateTab)),
                    (
                        if marked_c { "Unmark" } else { "Mark Important" },
                        AppMsg::Action(Action::ToggleTabMarked),
                    ),
                    ("Rename", AppMsg::Ignore),
                    ("Close", AppMsg::CloseTab(id_c)),
                ];
                for (label, msg) in entries {
                    let b = item(label);
                    let pop = popover.clone();
                    let s = s_c.clone();
                    let btn_for_rename = btn_c.clone();
                    let title_for_rename = title_c.clone();
                    b.connect_clicked(move |_| {
                        pop.popdown();
                        match &msg {
                            AppMsg::Ignore => {
                                // Rename: open a prefilled entry popover.
                                let rp = gtk::Popover::new();
                                rp.set_parent(&btn_for_rename);
                                let entry = gtk::Entry::new();
                                entry.set_text(&title_for_rename);
                                entry.select_region(0, -1);
                                rp.set_child(Some(&entry));
                                let s_e = s.clone();
                                let rp_c = rp.clone();
                                entry.connect_activate(move |e| {
                                    s_e.input(AppMsg::RenameTab(id_c, e.text().to_string()));
                                    rp_c.popdown();
                                });
                                rp.connect_closed(|p| p.unparent());
                                rp.popup();
                                entry.grab_focus();
                            }
                            AppMsg::Action(_) => {
                                // Target the clicked tab, then run the action.
                                s.input(AppMsg::SelectTab(id_c));
                                s.input(msg.clone());
                            }
                            _ => s.input(msg.clone()),
                        }
                    });
                    vbox.append(&b);
                }
                popover.set_child(Some(&vbox));
                popover.connect_closed(|p| p.unparent());
                popover.popup();
            });
            select_btn.add_controller(ctx);

            let close_btn = gtk::Button::with_label("✕");
            close_btn.add_css_class("tab-close");
            let id2 = tab.id;
            let s2 = sender.clone();
            close_btn.connect_clicked(move |_| s2.input(AppMsg::CloseTab(id2)));

            // Drag-and-drop reorder: drag a tab button, drop onto another row.
            let drag = gtk::DragSource::new();
            drag.set_actions(gtk::gdk::DragAction::MOVE);
            let drag_id = tab.id;
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&drag_id.to_value()))
            });
            select_btn.add_controller(drag);

            let drop = gtk::DropTarget::new(glib::Type::U64, gtk::gdk::DragAction::MOVE);
            let target_idx = idx;
            let s_d = sender.clone();
            drop.connect_drop(move |_, value, _, _| {
                if let Ok(src_id) = value.get::<u64>() {
                    s_d.input(AppMsg::ReorderTab(src_id, target_idx));
                    true
                } else {
                    false
                }
            });
            row.add_controller(drop);

            row.append(&select_btn);
            row.append(&close_btn);
            self.tab_strip.append(&row);
        }
        self.persist_session();
    }
}

fn install_static_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; margin-bottom: 2px; }
         .tab-strip-btn:checked { font-weight: bold; border: 1px solid currentColor; border-radius: 4px; }
         .tab-close { min-width: 16px; min-height: 16px; padding: 0; margin: 0; }
         .tab-strip { min-width: 140px; padding: 2px 4px; }
         .top-bar { padding: 2px 4px; }
         .terminal-box scrollbar slider { min-width: 6px; border-radius: 3px; }
         .terminal-box scrollbar { padding: 0; }
         .tab-activity { font-style: italic; }
         .tab-bell { color: #f1fa8c; }
         .tab-marked { background-color: rgba(80,160,255,0.22); font-weight: bold; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[relm4::component]
impl SimpleComponent for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("jterm1"),
            set_default_width: 800,
            set_default_height: 600,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,

                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    add_css_class: "top-bar",

                    gtk::Button {
                        set_label: "☰",
                        connect_clicked[sender] => move |_| sender.input(AppMsg::ToggleSidebar),
                    },
                    gtk::Box { set_hexpand: true },
                    gtk::Button {
                        set_label: "+",
                        connect_clicked[sender] => move |_| sender.input(AppMsg::NewTab),
                    },
                },

                #[local_ref]
                search_bar -> gtk::SearchBar {},

                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_vexpand: true,

                    // Sidebar: filter entry above the tab strip. Pin it
                    // non-expanding (width 160) so it does not compete 50/50
                    // with the terminal stack via hexpand propagation.
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_width_request: 160,
                        set_hexpand: false,
                        add_css_class: "tab-strip",
                        #[watch]
                        set_visible: model.sidebar_visible,

                        #[local_ref]
                        tab_filter_entry -> gtk::SearchEntry {},

                        #[local_ref]
                        tab_strip -> gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_vexpand: false,
                            set_hexpand: false,
                        },

                        gtk::Separator {
                            set_orientation: gtk::Orientation::Horizontal,
                        },

                        // File browser header: root label + up / goto-cwd.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_spacing: 2,

                            gtk::Button {
                                set_icon_name: "go-up-symbolic",
                                set_tooltip_text: Some("Parent directory"),
                                connect_clicked[sender] => move |_| sender.input(AppMsg::FileTreeGoUp),
                            },
                            gtk::Button {
                                set_icon_name: "go-home-symbolic",
                                set_tooltip_text: Some("Go to current directory"),
                                connect_clicked[sender] => move |_| sender.input(AppMsg::FileTreeGotoCwd),
                            },
                            #[local_ref]
                            file_tree_root_label -> gtk::Label {
                                set_hexpand: true,
                            },
                        },

                        #[local_ref]
                        file_tree_scroll -> gtk::ScrolledWindow {
                            set_vexpand: true,
                            set_hexpand: false,
                        },
                    },

                    #[local_ref]
                    stack -> gtk::Stack {
                        set_hexpand: true,
                        set_vexpand: true,
                    },
                },
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let (config, themes, kbmap) = load_config();
        let shell_argv = Rc::new(choose_shell_argv(config.shell.as_deref()));
        let startup = config.startup_commands.clone();
        let window_opacity = config.window_opacity;
        let font_scale = config.default_font_scale;
        let config = Rc::new(RefCell::new(config));

        root.set_opacity(window_opacity);

        install_static_css();
        let dyn_css = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &dyn_css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let stack = gtk::Stack::new();
        let tab_strip = gtk::Box::new(gtk::Orientation::Vertical, 0);

        // Inline find bar (hidden until ToggleSearch). The entry feeds queries
        // back to the model, which routes them to the active terminal backend.
        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Find… (/regex/ for regex)"));
        search_entry.set_hexpand(true);
        let search_bar = gtk::SearchBar::builder()
            .search_mode_enabled(false)
            .build();
        search_bar.set_child(Some(&search_entry));
        search_bar.connect_entry(&search_entry);
        {
            let sender = sender.clone();
            search_entry.connect_search_changed(move |e| {
                sender.input(AppMsg::SearchChanged(e.text().to_string()));
            });
        }
        {
            let sender = sender.clone();
            search_entry.connect_activate(move |_| sender.input(AppMsg::SearchNext));
        }
        {
            let sender = sender.clone();
            let key = gtk::EventControllerKey::new();
            key.set_propagation_phase(gtk::PropagationPhase::Capture);
            key.connect_key_pressed(move |_, keyval, _, state| {
                use gtk::gdk::Key;
                if keyval == Key::Escape {
                    sender.input(AppMsg::SearchClose);
                    return glib::Propagation::Stop;
                }
                if matches!(keyval, Key::Return | Key::KP_Enter)
                    && state.contains(ModifierType::SHIFT_MASK)
                {
                    sender.input(AppMsg::SearchPrev);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            search_entry.add_controller(key);
        }

        // Tab-strip filter entry (lives at the top of the sidebar). FilterTabs
        // focuses it; typing hides strip rows whose title doesn't match.
        let tab_filter_entry = gtk::SearchEntry::new();
        tab_filter_entry.set_placeholder_text(Some("Filter tabs..."));
        {
            let sender = sender.clone();
            tab_filter_entry.connect_search_changed(move |e| {
                sender.input(AppMsg::SetTabFilter(e.text().to_string()));
            });
        }

        // File tree browser (lower half of the sidebar).
        let file_tree_store = file_tree::new_store();
        let file_tree_view = file_tree::new_view(&file_tree_store);
        let file_tree_root_label = gtk::Label::new(Some("~"));
        file_tree_root_label.set_xalign(0.0);
        file_tree_root_label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        {
            // Lazy directory expansion: fill children on first expand.
            let store = file_tree_store.clone();
            file_tree_view.connect_row_expanded(move |_tv, iter, _path| {
                file_tree::on_expand(&store, iter);
            });
        }
        {
            // Activate: toggle directories, insert file paths into the terminal.
            let store = file_tree_store.clone();
            let sender = sender.clone();
            file_tree_view.connect_row_activated(move |tv, path, _col| {
                let Some(iter) = store.iter(path) else { return };
                let is_dir: bool = store
                    .get_value(&iter, file_tree::COL_IS_DIR as i32)
                    .get()
                    .unwrap_or(false);
                if is_dir {
                    if tv.row_expanded(path) {
                        tv.collapse_row(path);
                    } else {
                        tv.expand_row(path, false);
                    }
                    return;
                }
                let file_path: String = store
                    .get_value(&iter, file_tree::COL_PATH as i32)
                    .get()
                    .unwrap_or_default();
                if !file_path.is_empty() {
                    sender.input(AppMsg::FileTreeActivateFile(file_path));
                }
            });
        }
        let file_tree_scroll = gtk::ScrolledWindow::new();
        file_tree_scroll.set_vexpand(true);
        file_tree_scroll.set_child(Some(&file_tree_view));

        let mut model = AppModel {
            config,
            themes: Rc::new(themes),
            kbmap: Rc::new(RefCell::new(kbmap)),
            shell_argv,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            next_pane_id: 0,
            sidebar_visible: true,
            font_scale,
            window_opacity,
            stack: stack.clone(),
            tab_strip: tab_strip.clone(),
            window: root.clone(),
            dyn_css,
            search_bar: search_bar.clone(),
            search_entry: search_entry.clone(),
            tab_filter_entry: tab_filter_entry.clone(),
            tab_filter: String::new(),
            file_tree_store: file_tree_store.clone(),
            file_tree_root_label: file_tree_root_label.clone(),
            file_tree_root: Rc::new(RefCell::new(std::path::PathBuf::new())),
            command_palette_dialog: Rc::new(RefCell::new(None)),
            settings_dialog: Rc::new(RefCell::new(None)),
            debug_dashboard_dialog: Rc::new(RefCell::new(None)),
        };

        let widgets = view_output!();

        // Window-level key controller: intercept shortcuts before VTE.
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let kb = model.kbmap.clone();
            let ksender = sender.clone();
            key_controller.connect_key_pressed(move |_c, keyval, _kc, state| {
                let mods = state
                    & (ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK | ModifierType::ALT_MASK);
                let combo = KeyCombo {
                    modifiers: mods,
                    key: normalize_key(keyval),
                };
                if let Some(action) = kb.borrow().lookup(&combo) {
                    ksender.input(AppMsg::Action(action));
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        root.add_controller(key_controller);

        // Config file hot reload: watch config.toml for external changes.
        let config_path = config_file_path();
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let config_file = gio::File::for_path(&config_path);
        if let Ok(monitor) =
            config_file.monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>)
        {
            let rsender = sender.clone();
            let reload_pending = Rc::new(std::cell::Cell::new(false));
            monitor.connect_changed(move |_, _, _, event| {
                if matches!(
                    event,
                    gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created
                ) && !reload_pending.get()
                {
                    reload_pending.set(true);
                    let rsender = rsender.clone();
                    let pending = reload_pending.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                        pending.set(false);
                        rsender.input(AppMsg::ReloadConfig);
                    });
                }
            });
            unsafe { root.set_data("config-monitor", monitor) };
        }

        model.apply_dynamic_css();

        // Restore a previously-saved session if present (consume-on-start);
        // otherwise open a single fresh tab running startup_commands.
        match session::load_session() {
            Some(saved) => {
                for tab in &saved.tabs {
                    model.restore_tab(tab, &sender);
                }
                let active_id = model
                    .tabs
                    .get(saved.active.min(model.tabs.len().saturating_sub(1)))
                    .map(|t| t.id);
                if let Some(id) = active_id {
                    model.select_tab(id, &sender);
                }
            }
            None => model.add_tab(startup, &sender),
        }

        model.init_file_tree();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::NewTab => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, &sender);
            }
            AppMsg::CloseTab(id) => self.request_close_tab(id, &sender),
            AppMsg::ForceCloseTab(id) => self.close_tab(id, &sender),
            AppMsg::ForceClosePane(pane_id) => self.close_pane(pane_id, &sender),
            AppMsg::SelectTab(id) => self.select_tab(id, &sender),
            AppMsg::NextTab => self.switch_tab(1, &sender),
            AppMsg::PrevTab => self.switch_tab(-1, &sender),
            AppMsg::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
            }
            AppMsg::Action(action) => self.execute_action(action, &sender),
            AppMsg::ReloadConfig => self.reload_config(),
            AppMsg::PaneExited(_, pane_id) => self.close_pane(pane_id, &sender),
            AppMsg::PaneCwdChanged(_, pane_id, path) => {
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    self.tabs[ti].panes[pi].cwd = Some(path.clone());
                    if self.tabs[ti].active_pane == pi && !self.tabs[ti].custom_title {
                        let number = ti as u32 + 1;
                        self.tabs[ti].title = default_tab_title(number, Some(&path));
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::PaneFocused(_, pane_id) => {
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    self.tabs[ti].active_pane = pi;
                }
            }
            AppMsg::TitleChanged(_id, _title) => {}
            AppMsg::Bell(id) => {
                if let Some(idx) = self.index_of(id) {
                    if idx != self.active {
                        self.tabs[idx].bell = true;
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::Activity(id) => {
                if let Some(idx) = self.index_of(id) {
                    if idx != self.active && !self.tabs[idx].activity {
                        self.tabs[idx].activity = true;
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::SettingsTheme(idx) => {
                if let Some(theme) = self.themes.get(idx).cloned() {
                    {
                        let mut config = self.config.borrow_mut();
                        config.theme_name = theme.name.clone();
                        config.foreground = theme.foreground;
                        config.background = theme.background;
                        config.cursor = theme.cursor;
                        config.cursor_foreground = theme.cursor_foreground;
                        config.palette = theme.palette;
                    }
                    for tab in &self.tabs {
                        for pane in &tab.panes {
                            pane.terminal.emit(VteInput::ApplyTheme);
                        }
                    }
                    self.apply_dynamic_css();
                    config::save_config(&self.config.borrow());
                }
            }
            AppMsg::SettingsFontDesc(desc) => {
                self.config.borrow_mut().font_desc = desc.clone();
                for tab in &self.tabs {
                    for pane in &tab.panes {
                        pane.terminal.emit(VteInput::SetFont(desc.clone()));
                    }
                }
                config::save_config(&self.config.borrow());
            }
            AppMsg::SettingsFontScale(scale) => {
                self.set_font_scale_all(scale);
                self.config.borrow_mut().default_font_scale = scale;
                config::save_config(&self.config.borrow());
            }
            AppMsg::SettingsOpacity(opacity) => {
                self.set_window_opacity(opacity);
                self.config.borrow_mut().window_opacity = opacity;
                config::save_config(&self.config.borrow());
            }
            AppMsg::SettingsScrollback(lines) => {
                self.config.borrow_mut().terminal_scrollback_lines = lines;
                for tab in &self.tabs {
                    for pane in &tab.panes {
                        pane.terminal.emit(VteInput::SetScrollback(lines as i64));
                    }
                }
                config::save_config(&self.config.borrow());
            }
            AppMsg::SearchChanged(text) => {
                if let Some(t) = self.active_terminal() {
                    if text.is_empty() {
                        t.emit(VteInput::SearchClear);
                    } else {
                        let (query, use_regex) = Self::search_query(&text);
                        t.emit(VteInput::SearchSet(query, use_regex));
                    }
                }
            }
            AppMsg::SearchNext => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchNext);
                }
            }
            AppMsg::SearchPrev => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchPrev);
                }
            }
            AppMsg::SearchClose => self.toggle_search(),
            AppMsg::RenameTab(id, title) => {
                if let Some(idx) = self.index_of(id) {
                    let trimmed = title.trim();
                    if trimmed.is_empty() {
                        self.tabs[idx].custom_title = false;
                        let number = idx as u32 + 1;
                        let cwd = self.tabs[idx]
                            .panes
                            .get(self.tabs[idx].active_pane)
                            .and_then(|p| p.cwd.clone());
                        self.tabs[idx].title = default_tab_title(number, cwd.as_deref());
                    } else {
                        self.tabs[idx].title = trimmed.to_string();
                        self.tabs[idx].custom_title = true;
                    }
                    self.rebuild_tab_strip(&sender);
                }
            }
            AppMsg::ReorderTab(src_id, to_idx) => self.reorder_tab(src_id, to_idx, &sender),
            AppMsg::SetTabFilter(text) => {
                self.tab_filter = text;
                self.rebuild_tab_strip(&sender);
            }
            AppMsg::FileTreeActivateFile(path) => {
                if let Some(term) = self.active_terminal() {
                    let snippet = format!("{} ", file_tree::shell_quote(&path));
                    term.emit(VteInput::WriteInput(snippet.into_bytes()));
                    term.emit(VteInput::GrabFocus);
                }
            }
            AppMsg::FileTreeGotoCwd => self.file_tree_goto_current_cwd(),
            AppMsg::FileTreeGoUp => self.file_tree_go_up(),
            AppMsg::Ignore => {}
        }
    }
}

fn main() {
    let app = RelmApp::new("app.jterm1");
    app.run::<AppModel>(());
}
