//! Sidebar file browser: a lazy-loading `TreeView` rooted at the active tab's
//! working directory (falling back to `$HOME`). Directories expand on demand;
//! activating a file inserts its shell-quoted path into the active terminal.
//! Ports jterm4's `ui/file_tree.rs` to jterm1's relm4 structure.
//!
//! GTK4 deprecated the TreeView/TreeStore family in 4.10 in favor of the new
//! list/column views, but they remain fully functional and a ColumnView rewrite
//! is out of scope; suppress the deprecation lints module-wide.
#![allow(deprecated)]

use relm4::gtk;

use gtk::glib;
use gtk::prelude::*;
use gtk::{CellRendererPixbuf, CellRendererText, TreeIter, TreeStore, TreeView, TreeViewColumn};
use std::path::{Path, PathBuf};

// TreeStore column indices.
pub(crate) const COL_NAME: u32 = 0;
pub(crate) const COL_PATH: u32 = 1;
pub(crate) const COL_IS_DIR: u32 = 2;
pub(crate) const COL_ICON: u32 = 3;

/// A four-column store: display name, absolute path, is-directory, icon name.
pub(crate) fn new_store() -> TreeStore {
    TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::BOOL,
        glib::Type::STRING,
    ])
}

/// Build the headerless `TreeView` (icon + name in one column), no signals wired.
pub(crate) fn new_view(store: &TreeStore) -> TreeView {
    let view = TreeView::with_model(store);
    view.set_headers_visible(false);
    view.set_vexpand(true);

    let column = TreeViewColumn::new();
    let icon = CellRendererPixbuf::new();
    column.pack_start(&icon, false);
    column.add_attribute(&icon, "icon-name", COL_ICON as i32);
    let text = CellRendererText::new();
    column.pack_start(&text, true);
    column.add_attribute(&text, "text", COL_NAME as i32);
    view.append_column(&column);
    view
}

/// Insert one row per directory entry under `parent` (dirs first, then files,
/// case-insensitive). Directories get a placeholder child so the expander arrow
/// shows before they are loaded.
pub(crate) fn populate_dir(store: &TreeStore, parent: Option<&TreeIter>, dir: &Path) {
    let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = path.is_dir();
            entries.push((name, path, is_dir));
        }
    }
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // directories (true) first
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    for (name, path, is_dir) in entries {
        let icon = if is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        };
        let path_str = path.to_string_lossy().to_string();
        let iter = store.insert_with_values(
            parent,
            None,
            &[
                (COL_NAME, &name),
                (COL_PATH, &path_str),
                (COL_IS_DIR, &is_dir),
                (COL_ICON, &icon),
            ],
        );
        if is_dir {
            // Placeholder child (empty path) → expander shows, loaded lazily.
            store.insert_with_values(
                Some(&iter),
                None,
                &[
                    (COL_NAME, &""),
                    (COL_PATH, &""),
                    (COL_IS_DIR, &false),
                    (COL_ICON, &""),
                ],
            );
        }
    }
}

/// Lazily fill a directory row's real children on first expansion.
pub(crate) fn on_expand(store: &TreeStore, iter: &TreeIter) {
    // A not-yet-loaded directory has a single placeholder child (empty path).
    let Some(first_child) = store.iter_children(Some(iter)) else {
        return;
    };
    let child_path: String = store.get_value(&first_child, COL_PATH as i32).get().unwrap_or_default();
    if !child_path.is_empty() {
        return; // already populated
    }
    store.remove(&first_child);
    let dir_path: String = store.get_value(iter, COL_PATH as i32).get().unwrap_or_default();
    if !dir_path.is_empty() {
        populate_dir(store, Some(iter), Path::new(&dir_path));
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Abbreviate the home directory to `~` for the header label.
pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    path.to_string_lossy().to_string()
}

/// Single-quote a path for safe shell insertion.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
