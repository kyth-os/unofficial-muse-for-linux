//! File-write section for the Files page: separate write grants plus
//! per-operation inline confirmation.
//!
//! A read grant never authorizes writes: the user picks a write folder
//! explicitly (native chooser, guarded by the host's dialog lease), and
//! every mutation is staged as an exact preview card that needs a fresh
//! Confirm press. Nothing executes on staging; Cancel drops the staged
//! bytes. Grant revoke or replacement drops any staged op.
#![allow(unexpected_cfgs)]

#[cfg(not(files_write_controller_only))]
mod native {
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_linux_core::filesystem::{MAX_WRITE_BYTES, WriteGrant};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// Handle to the host's single-dialog lease, wired by the caller so the
    /// write-folder chooser never stacks over another native dialog.
    pub struct DialogGuard {
        pub is_active: Rc<dyn Fn() -> bool>,
        pub acquire: Rc<dyn Fn(gtk::FileChooserNative)>,
        pub release: Rc<dyn Fn()>,
    }

    /// Staged mutation awaiting fresh confirmation. Bytes live here only
    /// until Confirm executes or Cancel/revoke drops them.
    enum PendingOp {
        Create { rel: PathBuf, bytes: Vec<u8> },
        Overwrite { rel: PathBuf, bytes: Vec<u8> },
        Delete { rel: PathBuf },
        Rename { from: PathBuf, to: PathBuf },
        Mkdir { rel: PathBuf },
    }

    impl PendingOp {
        fn describe(&self) -> String {
            match self {
                PendingOp::Create { rel, bytes } => {
                    format!("Create file `{}` ({} bytes)", rel.display(), bytes.len())
                }
                PendingOp::Overwrite { rel, bytes } => {
                    format!("Overwrite file `{}` ({} bytes)", rel.display(), bytes.len())
                }
                PendingOp::Delete { rel } => format!("Delete file `{}`", rel.display()),
                PendingOp::Rename { from, to } => {
                    format!("Rename `{}` to `{}`", from.display(), to.display())
                }
                PendingOp::Mkdir { rel } => format!("Create folder `{}`", rel.display()),
            }
        }

        fn preview_bytes(&self) -> Option<&[u8]> {
            match self {
                PendingOp::Create { bytes, .. } | PendingOp::Overwrite { bytes, .. } => Some(bytes),
                _ => None,
            }
        }
    }

    // pub(super) so display smoke tests can reach staged state.
    pub(super) struct Ui {
        write_grant: RefCell<Option<WriteGrant>>,
        pending: RefCell<Option<PendingOp>>,
        write_location: gtk::Label,
        write_status: gtk::Label,
        rel: gtk::Entry,
        rel_to: gtk::Entry,
        content: gtk::TextView,
        confirm_card: gtk::Box,
        confirm_heading: gtk::Label,
        confirm_preview: gtk::TextView,
        refresh_files: Rc<dyn Fn()>,
    }

    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }

    fn text_of(view: &gtk::TextView) -> String {
        let buffer = view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    fn set_text(view: &gtk::TextView, text: &str) {
        view.buffer().set_text(text);
    }

    /// First 2 KiB of staged bytes as lossy text for the preview card.
    fn preview_text(bytes: &[u8]) -> String {
        const PREVIEW_CHARS: usize = 2048;
        let text = String::from_utf8_lossy(bytes);
        if text.chars().count() > PREVIEW_CHARS {
            let truncated: String = text.chars().take(PREVIEW_CHARS).collect();
            format!("{truncated}\n… (truncated, {} bytes total)", bytes.len())
        } else {
            format!("{text}\n({} bytes total)", bytes.len())
        }
    }

    impl Ui {
        fn note(&self, text: &str) {
            self.write_status.set_text(text);
        }

        fn rel_path(&self) -> Result<PathBuf, String> {
            let raw = self.rel.text().trim().to_string();
            if raw.is_empty() {
                return Err("Enter a relative file path first.".into());
            }
            Ok(PathBuf::from(raw))
        }

        fn content_bytes(&self) -> Result<Vec<u8>, String> {
            let bytes = text_of(&self.content).into_bytes();
            if bytes.len() as u64 > MAX_WRITE_BYTES {
                return Err(format!(
                    "Content is {} bytes; the write limit is {MAX_WRITE_BYTES} bytes.",
                    bytes.len()
                ));
            }
            Ok(bytes)
        }

        fn stage(&self, op: PendingOp) {
            self.pending.replace(Some(op));
            let pending = self.pending.borrow();
            let op = pending.as_ref().expect("just staged");
            self.confirm_heading
                .set_text(&format!("Confirm — {}", op.describe()));
            match op.preview_bytes() {
                Some(bytes) => set_text(&self.confirm_preview, &preview_text(bytes)),
                None => set_text(&self.confirm_preview, "(no file content involved)"),
            }
            self.confirm_card.set_visible(true);
            self.note("Staged. Nothing has been written yet — press Confirm to execute.");
        }

        fn cancel(&self) {
            self.pending.replace(None);
            self.confirm_card.set_visible(false);
            self.note("Staged change discarded. Nothing was written.");
        }

        fn confirm(&self) {
            let Some(op) = self.pending.take() else {
                self.note("Nothing staged to confirm.");
                return;
            };
            self.confirm_card.set_visible(false);
            let granted = self.write_grant.borrow();
            let Some(grant) = granted.as_ref() else {
                self.note(
                    "No write grant. Grant write access to a folder first — nothing was written.",
                );
                return;
            };
            let result = match &op {
                PendingOp::Create { rel, bytes } => grant.create_file(rel, bytes),
                PendingOp::Overwrite { rel, bytes } => grant.overwrite_file(rel, bytes),
                PendingOp::Delete { rel } => grant.delete_file(rel),
                PendingOp::Rename { from, to } => grant.rename(from, to),
                PendingOp::Mkdir { rel } => grant.create_dir(rel),
            };
            drop(granted);
            match result {
                Ok(()) => {
                    self.note(&format!("Done: {}.", op.describe()));
                    (self.refresh_files)();
                }
                Err(error) => {
                    self.note(&format!("Write refused: {error} Nothing was written."));
                }
            }
        }

        fn drop_grant(&self) {
            self.write_grant.replace(None);
            self.pending.replace(None);
            self.confirm_card.set_visible(false);
            self.write_location.set_text("No write access granted");
            self.note("Write access revoked. Staged changes (if any) were discarded.");
        }
    }

    /// Build the write section. `refresh_files` re-renders the host file
    /// list after a confirmed mutation.
    pub fn build_section<W: IsA<gtk::Window> + Clone>(
        parent: &W,
        guard: DialogGuard,
        refresh_files: Rc<dyn Fn()>,
    ) -> gtk::Box {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let heading = label("Write access (separate grant, every change confirmed)");
        heading.add_css_class("section-heading");
        root.append(&heading);
        root.append(&label(
            "Reading never authorizes writing. Grant a write folder explicitly; each create/overwrite/delete/rename needs a fresh Confirm on the exact preview. Revoking drops staged changes.",
        ));

        let grant_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        grant_row.add_css_class("toolbar");
        let grant_button = gtk::Button::with_label("Grant write access to a folder…");
        let revoke_button = gtk::Button::with_label("Revoke write access");
        let write_location = gtk::Label::new(Some("No write access granted"));
        write_location.set_xalign(0.0);
        write_location.set_selectable(true);
        write_location.set_hexpand(true);
        grant_row.append(&grant_button);
        grant_row.append(&revoke_button);
        grant_row.append(&write_location);
        root.append(&grant_row);

        let rel = gtk::Entry::new();
        rel.set_placeholder_text(Some("Relative path, e.g. notes/todo.txt"));
        root.append(&rel);
        let rel_to = gtk::Entry::new();
        rel_to.set_placeholder_text(Some("Rename destination (rename only)"));
        root.append(&rel_to);
        let content = gtk::TextView::new();
        content.set_widget_name("write-content");
        content.set_wrap_mode(gtk::WrapMode::WordChar);
        let content_scroll = gtk::ScrolledWindow::new();
        content_scroll.set_min_content_height(80);
        content_scroll.set_child(Some(&content));
        root.append(&content_scroll);

        let op_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        op_row.add_css_class("toolbar");
        let create = gtk::Button::with_label("Stage create");
        let overwrite = gtk::Button::with_label("Stage overwrite");
        let delete = gtk::Button::with_label("Stage delete");
        let rename = gtk::Button::with_label("Stage rename");
        let mkdir = gtk::Button::with_label("Stage new folder");
        for button in [&create, &overwrite, &delete, &rename, &mkdir] {
            op_row.append(button);
        }
        root.append(&op_row);

        let confirm_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        confirm_card.add_css_class("card");
        confirm_card.add_css_class("approval-card");
        let confirm_heading = label("Confirm");
        confirm_heading.add_css_class("approval-heading");
        confirm_card.append(&confirm_heading);
        let confirm_preview = gtk::TextView::new();
        confirm_preview.set_editable(false);
        confirm_preview.set_wrap_mode(gtk::WrapMode::WordChar);
        confirm_preview.set_monospace(true);
        let confirm_scroll = gtk::ScrolledWindow::new();
        confirm_scroll.set_min_content_height(80);
        confirm_scroll.set_child(Some(&confirm_preview));
        confirm_card.append(&confirm_scroll);
        let confirm_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let confirm = gtk::Button::with_label("Confirm");
        confirm.add_css_class("suggested-action");
        let cancel = gtk::Button::with_label("Cancel");
        confirm_row.append(&confirm);
        confirm_row.append(&cancel);
        confirm_card.append(&confirm_row);
        confirm_card.set_visible(false);
        root.append(&confirm_card);

        let write_status =
            gtk::Label::new(Some("No write grant. Writes stay disabled until granted."));
        write_status.set_xalign(0.0);
        write_status.set_wrap(true);
        root.append(&write_status);
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        root.append(&spinner);

        let ui = Rc::new(Ui {
            write_grant: RefCell::new(None),
            pending: RefCell::new(None),
            write_location,
            write_status,
            rel,
            rel_to,
            content,
            confirm_card,
            confirm_heading,
            confirm_preview,
            refresh_files,
        });

        {
            let ui = Rc::clone(&ui);
            let parent = parent.clone();
            grant_button.connect_clicked(move |_| {
                if (guard.is_active)() {
                    ui.note("Another dialog is open. Close it before granting write access.");
                    return;
                }
                let dialog = gtk::FileChooserNative::new(
                    Some("Choose a folder to grant write access"),
                    Some(&parent),
                    gtk::FileChooserAction::SelectFolder,
                    Some("Grant write access"),
                    Some("Cancel"),
                );
                let ui = Rc::clone(&ui);
                let release = Rc::clone(&guard.release);
                dialog.connect_response(move |chooser, response| {
                    if response == gtk::ResponseType::Accept {
                        match chooser.file().and_then(|file| file.path()) {
                            Some(path) => match WriteGrant::new(&path) {
                                Ok(new_grant) => {
                                    ui.write_location.set_text(&path.display().to_string());
                                    ui.write_grant.replace(Some(new_grant));
                                    ui.pending.replace(None);
                                    ui.confirm_card.set_visible(false);
                                    ui.note("Write access granted. Stage a change below; nothing executes without Confirm.");
                                }
                                Err(error) => {
                                    ui.note(&format!("Could not grant write access: {error}"));
                                }
                            },
                            None => ui.note("The selected location is not a local filesystem path."),
                        }
                    }
                    chooser.hide();
                    release();
                });
                (guard.acquire)(dialog.clone());
                dialog.show();
            });
        }
        {
            let ui = Rc::clone(&ui);
            revoke_button.connect_clicked(move |_| ui.drop_grant());
        }
        {
            let ui = Rc::clone(&ui);
            create.connect_clicked(move |_| match (ui.rel_path(), ui.content_bytes()) {
                (Ok(rel), Ok(bytes)) => ui.stage(PendingOp::Create { rel, bytes }),
                (Err(note), _) | (_, Err(note)) => ui.note(&note),
            });
        }
        {
            let ui = Rc::clone(&ui);
            overwrite.connect_clicked(move |_| match (ui.rel_path(), ui.content_bytes()) {
                (Ok(rel), Ok(bytes)) => ui.stage(PendingOp::Overwrite { rel, bytes }),
                (Err(note), _) | (_, Err(note)) => ui.note(&note),
            });
        }
        {
            let ui = Rc::clone(&ui);
            delete.connect_clicked(move |_| match ui.rel_path() {
                Ok(rel) => ui.stage(PendingOp::Delete { rel }),
                Err(note) => ui.note(&note),
            });
        }
        {
            let ui = Rc::clone(&ui);
            rename.connect_clicked(move |_| {
                let from = ui.rel_path();
                let to_raw = ui.rel_to.text().trim().to_string();
                match (from, to_raw.is_empty()) {
                    (Ok(from), false) => ui.stage(PendingOp::Rename {
                        from,
                        to: PathBuf::from(to_raw),
                    }),
                    (Err(note), _) => ui.note(&note),
                    (Ok(_), true) => ui.note("Enter a rename destination first."),
                }
            });
        }
        {
            let ui = Rc::clone(&ui);
            mkdir.connect_clicked(move |_| match ui.rel_path() {
                Ok(rel) => ui.stage(PendingOp::Mkdir { rel }),
                Err(note) => ui.note(&note),
            });
        }
        {
            let ui = Rc::clone(&ui);
            confirm.connect_clicked(move |_| ui.confirm());
        }
        {
            let ui = Rc::clone(&ui);
            cancel.connect_clicked(move |_| ui.cancel());
        }

        root
    }
}

#[cfg(not(files_write_controller_only))]
pub use native::{DialogGuard, build_section};

#[cfg(test)]
mod tests {
    use super::native::DialogGuard;
    use super::native::build_section;
    use gtk::prelude::*;
    use gtk4 as gtk;
    use std::rc::Rc;

    fn guard() -> DialogGuard {
        DialogGuard {
            is_active: Rc::new(|| false),
            acquire: Rc::new(|_: gtk::FileChooserNative| {}),
            release: Rc::new(|| {}),
        }
    }

    fn button(root: &gtk::Widget, name: &str) -> gtk::Button {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Ok(btn) = widget.clone().downcast::<gtk::Button>()
                && btn.label().as_deref() == Some(name)
            {
                return btn;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                stack.push(current.clone());
                child = current.next_sibling();
            }
        }
        panic!("button {name} not found")
    }

    fn has_label(root: &gtk::Widget, fragment: &str) -> bool {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Ok(lbl) = widget.clone().downcast::<gtk::Label>()
                && lbl.text().contains(fragment)
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                stack.push(current.clone());
                child = current.next_sibling();
            }
        }
        false
    }

    #[test]
    #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
    fn staging_validates_and_confirm_needs_a_grant() {
        gtk::init().unwrap();
        let window = gtk::Window::new();
        let section = build_section(&window, guard(), Rc::new(|| {}));
        let root: &gtk::Widget = section.upcast_ref();
        // Empty path refuses to stage.
        button(root, "Stage delete").emit_clicked();
        assert!(has_label(root, "Enter a relative file path"));
        // Confirm with nothing staged is a no-op notice.
        button(root, "Confirm").emit_clicked();
        assert!(has_label(root, "Nothing staged"));
        window.close();
    }
}
