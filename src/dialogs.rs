//! Command-palette and settings dialogs.
//!
//! Both are self-contained libadwaita dialogs presented over the main window.
//! User choices are dispatched back to `AppModel` as `AppMsg` via the component
//! sender (the relm4 way), rather than mutating model state from GTK closures.

use relm4::adw;
use relm4::gtk;
use relm4::ComponentSender;
use adw::prelude::*;
use gtk::gdk::Key;
use gtk::gdk::ModifierType;
use gtk::pango::FontDescription;
use std::cell::RefCell;
use std::rc::Rc;

use crate::config::{Config, Theme};
use crate::keybindings::{Action, KeybindingMap};
use crate::{AppModel, AppMsg};

/// Fuzzy-filterable list of every bound action. Activating a row dispatches the
/// action and closes the palette. A second invocation toggles it closed.
pub(crate) fn toggle_command_palette(
    window: &adw::ApplicationWindow,
    kbmap: &Rc<RefCell<KeybindingMap>>,
    dialog_ref: &Rc<RefCell<Option<adw::Dialog>>>,
    sender: &ComponentSender<AppModel>,
) {
    if let Some(dialog) = dialog_ref.borrow_mut().take() {
        dialog.force_close();
        return;
    }

    let bound = kbmap.borrow().all_bound_actions();
    let actions_data: Rc<Vec<(Action, String, String)>> = Rc::new(
        bound
            .iter()
            .map(|(action, binding)| (*action, action.name().to_string(), binding.clone()))
            .collect(),
    );

    let dialog = adw::Dialog::builder()
        .title("Command Palette")
        .content_width(480)
        .content_height(480)
        .build();

    let header_bar = adw::HeaderBar::new();
    let filter_entry = gtk::SearchEntry::new();
    filter_entry.set_placeholder_text(Some("Search commands..."));
    filter_entry.set_hexpand(true);
    filter_entry.set_margin_start(12);
    filter_entry.set_margin_end(12);
    filter_entry.set_margin_top(8);
    filter_entry.set_margin_bottom(8);

    let list_box = gtk::ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.set_margin_bottom(12);

    for (_, description, binding) in actions_data.iter() {
        let row = adw::ActionRow::builder()
            .title(description.as_str())
            .activatable(true)
            .build();
        if !binding.is_empty() {
            let key_label = gtk::Label::new(Some(binding));
            key_label.add_css_class("dim-label");
            row.add_suffix(&key_label);
        }
        list_box.append(&row);
    }
    if let Some(first_row) = list_box.row_at_index(0) {
        list_box.select_row(Some(&first_row));
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&list_box)
        .build();

    let search_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    search_box.append(&filter_entry);
    search_box.append(&scrolled);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&search_box));
    dialog.set_child(Some(&toolbar_view));

    // Substring filter over description + binding.
    {
        let list_box = list_box.clone();
        let actions_data = actions_data.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            let mut first_visible: Option<gtk::ListBoxRow> = None;
            for (idx, (_, desc, binding)) in actions_data.iter().enumerate() {
                if let Some(row) = list_box.row_at_index(idx as i32) {
                    let visible = query.is_empty()
                        || desc.to_lowercase().contains(&query)
                        || binding.to_lowercase().contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            if let Some(row) = first_visible {
                list_box.select_row(Some(&row));
            }
        });
    }

    let fire: Rc<dyn Fn(usize)> = {
        let sender = sender.clone();
        let actions_data = actions_data.clone();
        let dialog = dialog.clone();
        Rc::new(move |idx: usize| {
            if let Some((action, _, _)) = actions_data.get(idx) {
                let action = *action;
                dialog.force_close();
                sender.input(AppMsg::Action(action));
            }
        })
    };

    {
        let fire = fire.clone();
        list_box.connect_row_activated(move |_, row| fire(row.index() as usize));
    }

    // Escape closes; Enter fires the selected row; Up/Down navigate while the
    // search entry keeps keyboard focus.
    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog_ref = dialog_ref.clone();
        let list_box = list_box.clone();
        let fire = fire.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == Key::Escape
                || (matches!(keyval, Key::P | Key::p)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                if let Some(d) = dialog_ref.borrow_mut().take() {
                    d.force_close();
                }
                return gtk::glib::Propagation::Stop;
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box.selected_row() {
                    fire(row.index() as usize);
                }
                return gtk::glib::Propagation::Stop;
            }
            if keyval == Key::Down {
                let cur = list_box.selected_row().map(|r| r.index()).unwrap_or(-1);
                let mut next = cur + 1;
                while let Some(row) = list_box.row_at_index(next) {
                    if row.is_visible() {
                        list_box.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return gtk::glib::Propagation::Stop;
            }
            if keyval == Key::Up {
                let cur = list_box.selected_row().map(|r| r.index()).unwrap_or(0);
                let mut prev = cur - 1;
                while prev >= 0 {
                    if let Some(row) = list_box.row_at_index(prev) {
                        if row.is_visible() {
                            list_box.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dialog.add_controller(key_controller);

    {
        let dialog_ref = dialog_ref.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });
    }

    *dialog_ref.borrow_mut() = Some(dialog.clone());
    dialog.present(Some(window));
    filter_entry.grab_focus();
}

/// Searchable list of configured remote hosts. Activating a row opens a new tab
/// that connects to that host via ssh (dispatched as `ConnectRemote(idx)`).
pub(crate) fn show_remote_picker(
    window: &adw::ApplicationWindow,
    config: &Rc<RefCell<Config>>,
    sender: &ComponentSender<AppModel>,
) {
    let hosts = config.borrow().remote_hosts.clone();
    if hosts.is_empty() {
        log::warn!("[remote] no remote_hosts configured; nothing to pick");
        return;
    }

    let dialog = adw::Dialog::builder()
        .title("Connect to Remote Host")
        .content_width(480)
        .content_height(480)
        .build();

    let header_bar = adw::HeaderBar::new();
    let filter_entry = gtk::SearchEntry::new();
    filter_entry.set_placeholder_text(Some("Search hosts..."));
    filter_entry.set_hexpand(true);
    filter_entry.set_margin_start(12);
    filter_entry.set_margin_end(12);
    filter_entry.set_margin_top(8);
    filter_entry.set_margin_bottom(8);

    let list_box = gtk::ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.set_margin_bottom(12);

    let haystacks: Rc<Vec<String>> = Rc::new(
        hosts
            .iter()
            .map(|h| {
                let target = match &h.user {
                    Some(u) => format!("{u}@{}", h.host),
                    None => h.host.clone(),
                };
                format!("{} {}", h.name, target).to_lowercase()
            })
            .collect(),
    );

    for h in hosts.iter() {
        let target = match &h.user {
            Some(u) => format!("{u}@{}", h.host),
            None => h.host.clone(),
        };
        let row = adw::ActionRow::builder()
            .title(h.name.as_str())
            .subtitle(target.as_str())
            .activatable(true)
            .build();
        list_box.append(&row);
    }
    if let Some(first_row) = list_box.row_at_index(0) {
        list_box.select_row(Some(&first_row));
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&list_box)
        .build();

    let search_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    search_box.append(&filter_entry);
    search_box.append(&scrolled);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&search_box));
    dialog.set_child(Some(&toolbar_view));

    {
        let list_box = list_box.clone();
        let haystacks = haystacks.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            let mut first_visible: Option<gtk::ListBoxRow> = None;
            for (idx, hay) in haystacks.iter().enumerate() {
                if let Some(row) = list_box.row_at_index(idx as i32) {
                    let visible = query.is_empty() || hay.contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            if let Some(row) = first_visible {
                list_box.select_row(Some(&row));
            }
        });
    }

    let fire: Rc<dyn Fn(usize)> = {
        let sender = sender.clone();
        let dialog = dialog.clone();
        Rc::new(move |idx: usize| {
            dialog.force_close();
            sender.input(AppMsg::Action(Action::ConnectRemote(idx as u8)));
        })
    };

    {
        let fire = fire.clone();
        list_box.connect_row_activated(move |_, row| fire(row.index() as usize));
    }

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog = dialog.clone();
        let list_box = list_box.clone();
        let fire = fire.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, _state| {
            if keyval == Key::Escape {
                dialog.force_close();
                return gtk::glib::Propagation::Stop;
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box.selected_row() {
                    fire(row.index() as usize);
                }
                return gtk::glib::Propagation::Stop;
            }
            if keyval == Key::Down {
                let cur = list_box.selected_row().map(|r| r.index()).unwrap_or(-1);
                let mut next = cur + 1;
                while let Some(row) = list_box.row_at_index(next) {
                    if row.is_visible() {
                        list_box.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return gtk::glib::Propagation::Stop;
            }
            if keyval == Key::Up {
                let cur = list_box.selected_row().map(|r| r.index()).unwrap_or(0);
                let mut prev = cur - 1;
                while prev >= 0 {
                    if let Some(row) = list_box.row_at_index(prev) {
                        if row.is_visible() {
                            list_box.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dialog.add_controller(key_controller);

    dialog.present(Some(window));
    filter_entry.grab_focus();
}

/// Read-only diagnostics overlay. Sections of key/value rows are rendered as
/// adw preference groups; a second invocation toggles it closed.
pub(crate) fn toggle_debug_dashboard(
    window: &adw::ApplicationWindow,
    info: Vec<(String, Vec<(String, String)>)>,
    dialog_ref: &Rc<RefCell<Option<adw::Dialog>>>,
) {
    if let Some(dialog) = dialog_ref.borrow_mut().take() {
        dialog.force_close();
        return;
    }

    let dialog = adw::Dialog::builder()
        .title("Debug Dashboard")
        .content_width(480)
        .content_height(560)
        .build();

    let header_bar = adw::HeaderBar::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);

    for (section, rows) in &info {
        let group = adw::PreferencesGroup::new();
        group.set_title(section);
        for (key, value) in rows {
            let row = adw::ActionRow::builder().title(key.as_str()).build();
            let value_label = gtk::Label::new(Some(value));
            value_label.add_css_class("dim-label");
            value_label.set_selectable(true);
            value_label.set_xalign(1.0);
            row.add_suffix(&value_label);
            group.add(&row);
        }
        content.append(&group);
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&content)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&scrolled));
    dialog.set_child(Some(&toolbar_view));

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog_ref = dialog_ref.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == Key::Escape || keyval == Key::F12 {
                if let Some(d) = dialog_ref.borrow_mut().take() {
                    d.force_close();
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dialog.add_controller(key_controller);

    {
        let dialog_ref = dialog_ref.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });
    }

    *dialog_ref.borrow_mut() = Some(dialog.clone());
    dialog.present(Some(window));
}

/// Live settings panel: theme, font, font size, font scale, opacity, scrollback.
/// Each change dispatches an `AppMsg` that the model applies and persists.
pub(crate) fn toggle_settings(
    window: &adw::ApplicationWindow,
    config: &Rc<RefCell<Config>>,
    themes: &Rc<Vec<Theme>>,
    font_scale: f64,
    window_opacity: f64,
    dialog_ref: &Rc<RefCell<Option<adw::PreferencesDialog>>>,
    sender: &ComponentSender<AppModel>,
) {
    if let Some(dialog) = dialog_ref.borrow_mut().take() {
        dialog.force_close();
        return;
    }

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Settings");
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();

    let cfg = config.borrow();

    // Theme.
    let theme_names: Vec<String> = themes.iter().map(|t| t.name.clone()).collect();
    let theme_model =
        gtk::StringList::new(&theme_names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .model(&theme_model)
        .build();
    let current_theme_idx = themes
        .iter()
        .position(|t| t.name == cfg.theme_name)
        .unwrap_or(0);
    theme_row.set_selected(current_theme_idx as u32);
    group.add(&theme_row);

    // Monospace font family.
    let pango_ctx = window.pango_context();
    let mut mono_fonts: Vec<String> = pango_ctx
        .list_families()
        .iter()
        .filter(|f| f.is_monospace())
        .map(|f| f.name().to_string())
        .collect();
    mono_fonts.sort_by_key(|a| a.to_lowercase());

    let current_font_desc = FontDescription::from_string(&cfg.font_desc);
    let current_family = current_font_desc
        .family()
        .map(|f| f.to_string())
        .unwrap_or_default();
    let font_model =
        gtk::StringList::new(&mono_fonts.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let font_row = adw::ComboRow::builder()
        .title("Font")
        .model(&font_model)
        .build();
    let current_font_idx = mono_fonts.iter().position(|f| f == &current_family).unwrap_or(0);
    font_row.set_selected(current_font_idx as u32);
    group.add(&font_row);

    // Font size (points).
    let current_size = current_font_desc.size() as f64 / gtk::pango::SCALE as f64;
    let font_size_adj = gtk::Adjustment::new(current_size.max(6.0), 6.0, 72.0, 1.0, 4.0, 0.0);
    let font_size_row = adw::SpinRow::new(Some(&font_size_adj), 1.0, 0);
    font_size_row.set_title("Font Size");
    group.add(&font_size_row);

    // Font scale.
    let font_scale_adj = gtk::Adjustment::new(font_scale, 0.1, 10.0, 0.025, 0.1, 0.0);
    let font_scale_row = adw::SpinRow::new(Some(&font_scale_adj), 0.025, 3);
    font_scale_row.set_title("Font Scale");
    group.add(&font_scale_row);

    // Opacity.
    let opacity_row = adw::ActionRow::builder().title("Opacity").build();
    let opacity_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.01, 1.0, 0.025);
    opacity_scale.set_value(window_opacity);
    opacity_scale.set_hexpand(true);
    opacity_scale.set_size_request(180, -1);
    opacity_row.add_suffix(&opacity_scale);
    group.add(&opacity_row);

    // Scrollback.
    let scrollback_adj = gtk::Adjustment::new(
        cfg.terminal_scrollback_lines as f64,
        0.0,
        1_000_000.0,
        100.0,
        1000.0,
        0.0,
    );
    let scrollback_row = adw::SpinRow::new(Some(&scrollback_adj), 100.0, 0);
    scrollback_row.set_title("Scrollback Lines");
    group.add(&scrollback_row);

    page.add(&group);
    dialog.add(&page);
    drop(cfg);

    // Signal wiring → dispatch AppMsg.
    {
        let sender = sender.clone();
        theme_row.connect_selected_notify(move |row| {
            sender.input(AppMsg::SettingsTheme(row.selected() as usize));
        });
    }
    {
        let sender = sender.clone();
        let mono_fonts = mono_fonts.clone();
        let font_size_row = font_size_row.clone();
        font_row.connect_selected_notify(move |row| {
            if let Some(family) = mono_fonts.get(row.selected() as usize) {
                let size = font_size_row.value() as i32;
                sender.input(AppMsg::SettingsFontDesc(format!("{family} {size}")));
            }
        });
    }
    {
        let sender = sender.clone();
        let mono_fonts = mono_fonts.clone();
        let font_row = font_row.clone();
        font_size_row.connect_value_notify(move |row| {
            let family = mono_fonts
                .get(font_row.selected() as usize)
                .map(|s| s.as_str())
                .unwrap_or("Monospace");
            let size = row.value() as i32;
            sender.input(AppMsg::SettingsFontDesc(format!("{family} {size}")));
        });
    }
    {
        let sender = sender.clone();
        font_scale_row.connect_value_notify(move |row| {
            sender.input(AppMsg::SettingsFontScale(row.value()));
        });
    }
    {
        let sender = sender.clone();
        opacity_scale.connect_value_changed(move |scale| {
            sender.input(AppMsg::SettingsOpacity(scale.value()));
        });
    }
    {
        let sender = sender.clone();
        scrollback_row.connect_value_notify(move |row| {
            sender.input(AppMsg::SettingsScrollback(row.value() as u32));
        });
    }

    {
        let dialog_ref = dialog_ref.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });
    }

    *dialog_ref.borrow_mut() = Some(dialog.clone());
    dialog.present(Some(window));
}
