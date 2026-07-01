//! Relm4 adapter around the jterm4 block view.
//!
//! jterm1's application shell expects terminal backends to be Relm4
//! components that speak `VteInit`/`VteInput`/`VteOutput`. The block-mode
//! implementation itself is now the jterm4 `block_view::TermView`; this file
//! only adapts that GTK view to the existing jterm1 component surface.

use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use relm4::prelude::*;
use vte4::prelude::TerminalExt;

use crate::block_view::TermView;

pub use super::vte::{VteInit, VteInput, VteOutput};

pub struct BlockTerminal {
    view: TermView,
}

impl Component for BlockTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk4::Widget;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast()
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let view = TermView::new(
            &init.config.borrow(),
            init.shell_argv.as_ref().as_slice(),
            init.working_directory.as_deref(),
            init.session_id.as_deref(),
            init.initial_commands.as_deref(),
        );

        init.probe.shell_pid.set(view.pid_i32());
        init.probe.pty_fd.set(view.pty_fd_i32());

        view.connect_cwd_changed({
            let sender = sender.clone();
            move |cwd| {
                let _ = sender.output(VteOutput::CwdChanged(cwd.to_string()));
            }
        });
        view.connect_remote_session_id({
            let sender = sender.clone();
            move |id| {
                let _ = sender.output(VteOutput::RemoteSessionId(id.to_string()));
            }
        });
        view.connect_exited({
            let sender = sender.clone();
            move |code| {
                let _ = sender.output(VteOutput::Exited(code));
            }
        });
        view.connect_bell({
            let sender = sender.clone();
            move || {
                let _ = sender.output(VteOutput::Bell);
            }
        });
        view.connect_title_changed({
            let sender = sender.clone();
            move |title| {
                let _ = sender.output(VteOutput::TitleChanged(title.to_string()));
            }
        });
        view.connect_activity({
            let sender = sender.clone();
            move || {
                let _ = sender.output(VteOutput::Activity);
            }
        });
        view.connect_block_finished({
            let sender = sender.clone();
            move |command, exit_code, output_sample| {
                let _ = sender.output(VteOutput::BlockFinished {
                    command,
                    exit_code,
                    output_sample,
                });
            }
        });

        if let Some(container) = root.downcast_ref::<gtk4::Box>() {
            container.append(&view.widget());
        }

        let model = BlockTerminal { view };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            VteInput::WriteInput(data) => self.view.write_input(&data),
            VteInput::Resize(cols, rows) => self.view.resize(cols, rows),
            VteInput::GrabFocus => self.view.grab_focus(),
            VteInput::Copy => self.view.copy_to_clipboard(),
            VteInput::CopyOutputOnly => self.view.copy_to_clipboard_with_modifier(true),
            VteInput::Paste => self.view.paste_from_clipboard(),
            VteInput::SetFontScale(scale) => self.view.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let font = FontDescription::from_string(&desc);
                self.view.set_font(&font);
            }
            VteInput::SetScrollback(lines) => self.view.vte().set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => self.view.scroll_lines(lines),
            VteInput::ApplyTheme => self.view.apply_theme(),
            VteInput::Kill => self.view.kill(),
            VteInput::FilterFailedBlocks => self.view.apply_failed_filter(),
            VteInput::FilterSlowBlocks => self.view.apply_slow_filter(),
            VteInput::FilterPinnedBlocks => self.view.apply_pinned_filter(),
            VteInput::ClearBlockFilter => self.view.clear_block_filter(),
            VteInput::JumpToPrevPinned => self.view.jump_to_pinned(-1),
            VteInput::JumpToNextPinned => self.view.jump_to_pinned(1),
            VteInput::SearchSet(query, use_regex) => {
                let _ = self.view.find_in_blocks(&query, use_regex);
            }
            VteInput::SearchNext => {
                let _ = self.view.find_next();
            }
            VteInput::SearchPrev => {
                let _ = self.view.find_prev();
            }
            VteInput::SearchClear => self.view.clear_find(),
        }
    }
}
