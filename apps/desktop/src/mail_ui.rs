//! Generic mail UI: provider settings, folder/message browsing, approval-gated compose.
//!
//! Passwords live only in a non-echo entry, are passed per call on worker
//! threads, and are never stored. No background polling: every network op
//! needs an explicit button press. Sends need fresh per-send approval on the
//! exact preview bytes; receipts and uncertain outcomes surface as notices,
//! never silently.
#![allow(unexpected_cfgs)]

#[cfg(not(mail_controller_only))]
#[allow(clippy::too_many_lines)]
mod native {
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_mail::{
        DraftController, Envelope, FolderStat, MailAccount, MailClient, MailDraft, MailError,
        MessageText, SendReceipt,
    };
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    pub struct MailPage {
        pub root: gtk::Box,
        #[cfg(test)]
        pub(super) ui: Rc<Ui>,
    }

    #[derive(Clone)]
    struct Snapshot {
        account: MailAccount,
        password: String,
    }

    enum Reply {
        Envelopes(u64, Result<Vec<Envelope>, MailError>),
        Body(u64, Result<MessageText, MailError>),
        Sent(u64, Result<SendReceipt, MailError>),
    }

    // pub(super) so the display smoke tests can stage UI state.
    pub(super) struct Ui {
        epoch: Cell<u64>,
        // pub(crate) so the display smoke test can stage the busy state.
        pub(crate) busy: Cell<bool>,
        snapshot: RefCell<Option<Snapshot>>,
        folders: RefCell<Vec<FolderStat>>,
        envelopes: RefCell<Vec<Envelope>>,
        controller: RefCell<DraftController>,
        host: gtk::Entry,
        port: gtk::Entry,
        username: gtk::Entry,
        password: gtk::Entry,
        connection_notice: gtk::Label,
        folder_combo: gtk::ComboBoxText,
        list_notice: gtk::Label,
        messages: gtk::ListBox,
        read_notice: gtk::Label,
        body_view: gtk::TextView,
        to: gtk::Entry,
        subject: gtk::Entry,
        compose_body: gtk::TextView,
        send_notice: gtk::Label,
        approval_card: gtk::Box,
        preview: gtk::TextView,
        spinner: gtk::Spinner,
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

    impl Ui {
        fn set_busy(&self, busy: bool) {
            self.busy.set(busy);
            if busy {
                self.spinner.start();
            } else {
                self.spinner.stop();
            }
        }

        fn snapshot(&self) -> Result<Snapshot, MailError> {
            let host = self.host.text().trim().to_string();
            if host.is_empty() || host.len() > 253 || host.contains([' ', '\r', '\n']) {
                return Err(MailError::Validation(
                    "Enter the provider IMAP host explicitly (no background lookup).".into(),
                ));
            }
            let port: u16 = self.port.text().trim().parse().map_err(|_| {
                MailError::Validation("IMAP port must be a number 1..=65535.".into())
            })?;
            if port == 0 {
                return Err(MailError::Validation("IMAP port must not be zero.".into()));
            }
            let username = self.username.text().trim().to_string();
            if username.is_empty() {
                return Err(MailError::Validation("Enter the account username.".into()));
            }
            let password = self.password.text().to_string();
            if password.is_empty() {
                return Err(MailError::Validation(
                    "Enter the password (kept in memory only, never stored).".into(),
                ));
            }
            Ok(Snapshot {
                account: MailAccount::new(&host, port, &username),
                password,
            })
        }

        fn current_folder(&self) -> Option<String> {
            self.folder_combo.active_text().map(|s| s.to_string())
        }

        fn refresh_folders(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.connection_notice
                    .set_text("A mail request is already running. Wait for it to finish first.");
                return;
            }
            let snapshot = match self.snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.connection_notice.set_text(&error.to_string());
                    return;
                }
            };
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            self.set_busy(true);
            self.connection_notice
                .set_text("Connecting with mandatory TLS; plaintext is refused…");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let folders = MailClient::connect_imap(&snapshot.account, &snapshot.password);
                let _ = tx.send((snapshot, folders));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok((snapshot, result)) => {
                        if ui.epoch.get() != epoch {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(folders) => {
                                ui.snapshot.replace(Some(snapshot));
                                ui.folder_combo.remove_all();
                                for folder in &folders {
                                    ui.folder_combo.append_text(&folder.name);
                                }
                                if !folders.is_empty() {
                                    ui.folder_combo.set_active(Some(0));
                                }
                                ui.folders.replace(folders);
                                ui.connection_notice.set_text(
                                    "Connected. Pick a folder and press Refresh messages (no background polling).",
                                );
                            }
                            Err(error) => {
                                ui.connection_notice.set_text(&format!(
                                    "Connection failed: {error} Nothing was cached."
                                ));
                            }
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.epoch.get() == epoch {
                            ui.set_busy(false);
                            ui.connection_notice
                                .set_text("Mail worker stopped unexpectedly; nothing was cached.");
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        }

        fn refresh_messages(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.list_notice
                    .set_text("A mail request is already running. Wait for it to finish first.");
                return;
            }
            let Some(snapshot) = self.snapshot.borrow().clone() else {
                self.list_notice
                    .set_text("Not connected. Connect first with an explicit button press.");
                return;
            };
            let Some(folder) = self.current_folder() else {
                self.list_notice.set_text("No folder selected.");
                return;
            };
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            self.set_busy(true);
            self.list_notice
                .set_text("Reading headers (bounded to 50)…");
            let (tx, rx) = mpsc::channel::<Reply>();
            std::thread::spawn(move || {
                let envelopes = MailClient::list(&account, &password, &folder, 50);
                let _ = tx.send(Reply::Envelopes(epoch, envelopes));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Envelopes(reply_epoch, result)) => {
                        if ui.epoch.get() != reply_epoch {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(envelopes) => {
                                while let Some(row) = ui.messages.last_child() {
                                    ui.messages.remove(&row);
                                }
                                for envelope in &envelopes {
                                    let row = gtk::Label::new(Some(&format!(
                                        "{} — {}",
                                        envelope.from, envelope.subject
                                    )));
                                    row.set_xalign(0.0);
                                    row.set_wrap(true);
                                    ui.messages.append(&row);
                                }
                                ui.envelopes.replace(envelopes);
                                ui.list_notice.set_text(
                                    "Headers loaded after your explicit refresh. Nothing polls in the background.",
                                );
                            }
                            Err(error) => {
                                ui.list_notice.set_text(&format!("Refresh failed: {error}"));
                            }
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(_) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.epoch.get() == epoch {
                            ui.set_busy(false);
                            ui.list_notice
                                .set_text("Mail worker stopped unexpectedly; list unchanged.");
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        }

        fn read_selected(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.read_notice
                    .set_text("A mail request is already running. Wait for it to finish first.");
                return;
            }
            let Some(snapshot) = self.snapshot.borrow().clone() else {
                self.read_notice
                    .set_text("Not connected. Connect first with an explicit button press.");
                return;
            };
            let Some(folder) = self.current_folder() else {
                self.read_notice.set_text("No folder selected.");
                return;
            };
            let selected = self.messages.selected_row().and_then(|row| {
                let index = row.index() as usize;
                self.envelopes.borrow().get(index).map(|e| e.uid)
            });
            let Some(uid) = selected else {
                self.read_notice
                    .set_text("Select a message first; nothing is fetched automatically.");
                return;
            };
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            self.set_busy(true);
            self.read_notice.set_text("Reading plain-text body…");
            let (tx, rx) = mpsc::channel::<Reply>();
            std::thread::spawn(move || {
                let body = MailClient::read_text(&account, &password, &folder, uid);
                let _ = tx.send(Reply::Body(epoch, body));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Body(reply_epoch, result)) => {
                        if ui.epoch.get() != reply_epoch {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(message) => {
                                set_text(&ui.body_view, &message.text);
                                ui.read_notice.set_text(if message.truncated {
                                    "Plain-text body shown, truncated at the 64 KiB cap; the remainder was not fetched. HTML-only messages are rejected, never rendered."
                                } else {
                                    "Plain-text body shown (HTML-only messages are rejected, never rendered)."
                                });
                            }
                            Err(error) => {
                                ui.read_notice.set_text(&format!("Read failed: {error}"));
                            }
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(_) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.epoch.get() == epoch {
                            ui.set_busy(false);
                            ui.read_notice
                                .set_text("Mail worker stopped unexpectedly; body unchanged.");
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        }

        fn stage_draft(&self) {
            let to_text = self.to.text();
            let subject_text = self.subject.text();
            let draft = MailDraft::new(
                to_text.trim(),
                subject_text.trim(),
                &text_of(&self.compose_body),
            );
            let draft = match draft {
                Ok(draft) => draft,
                Err(error) => {
                    self.send_notice
                        .set_text(&format!("Draft rejected: {error}"));
                    return;
                }
            };
            let staged = self.controller.borrow_mut().stage(draft);
            match staged {
                Ok(preview) => {
                    set_text(&self.preview, &String::from_utf8_lossy(&preview));
                    self.approval_card.set_visible(true);
                    self.send_notice.set_text(self.controller.borrow().notice());
                }
                Err(error) => {
                    self.send_notice
                        .set_text(&format!("Draft rejected: {error}"));
                }
            }
        }

        fn approve_staged(&self, ui: &Rc<Ui>) {
            if self.busy.get() {
                self.send_notice
                    .set_text("A mail request is already running. Wait for it to finish first.");
                return;
            }
            let Some(snapshot) = self.snapshot.borrow().clone() else {
                self.send_notice
                    .set_text("Not connected. Connect first; approval needs a live account.");
                return;
            };
            let Some(staged) = self.controller.borrow_mut().take_for_send() else {
                self.send_notice
                    .set_text("Nothing staged to approve; stage a draft first.");
                return;
            };
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            self.epoch.set(self.epoch.get().wrapping_add(1));
            let epoch = self.epoch.get();
            self.set_busy(true);
            self.approval_card.set_visible(false);
            self.send_notice.set_text(self.controller.borrow().notice());
            let (tx, rx) = mpsc::channel::<Reply>();
            std::thread::spawn(move || {
                let receipt = muse_mail::approve_send(&account, &password, &staged);
                let _ = tx.send(Reply::Sent(epoch, receipt));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Sent(reply_epoch, result)) => {
                        if ui.epoch.get() != reply_epoch {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(receipt) => ui.controller.borrow_mut().finish_sent(receipt),
                            Err(error) => ui.controller.borrow_mut().finish_failed(error),
                        }
                        ui.send_notice.set_text(ui.controller.borrow().notice());
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(_) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.epoch.get() == epoch {
                            ui.set_busy(false);
                            ui.controller.borrow_mut().finish_failed(MailError::Uncertain(
                                "Send worker stopped with no confirmation; delivery is unknown. Do not retry automatically.".into(),
                            ));
                            ui.send_notice.set_text(ui.controller.borrow().notice());
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        }

        fn deny_staged(&self) {
            self.controller.borrow_mut().deny();
            self.approval_card.set_visible(false);
            self.send_notice.set_text(self.controller.borrow().notice());
        }

        fn forget(&self) {
            if self.busy.get() {
                self.connection_notice.set_text(
                    "A mail request is already running. Wait for it to finish before disconnecting.",
                );
                return;
            }
            self.epoch.set(self.epoch.get().wrapping_add(1));
            self.set_busy(false);
            self.controller.borrow_mut().deny();
            self.snapshot.replace(None);
            self.folders.replace(Vec::new());
            self.envelopes.replace(Vec::new());
            self.folder_combo.remove_all();
            while let Some(row) = self.messages.last_child() {
                self.messages.remove(&row);
            }
            set_text(&self.body_view, "");
            self.password.set_text("");
            self.approval_card.set_visible(false);
            self.connection_notice.set_text(
                "Disconnected. Password cleared from memory; folders and messages dropped.",
            );
            self.list_notice.set_text("No folder data.");
            self.read_notice.set_text("Nothing selected.");
            self.send_notice.set_text(self.controller.borrow().notice());
        }
    }

    pub fn build() -> MailPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label(
            "Mail · generic IMAP reading, approval-gated SMTP sending",
        ));

        let settings = gtk::Expander::new(Some(
            "Mail provider connection (generic IMAP/SMTP, no OAuth)",
        ));
        settings.add_css_class("settings-disclosure");
        settings.set_expanded(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("settings-content");
        content.append(&label(
            "Any provider via explicit host and port. Passwords stay in memory only and are never stored; TLS is mandatory and plaintext is refused.",
        ));
        let host = gtk::Entry::new();
        host.set_placeholder_text(Some("IMAP host, e.g. imap.example.com"));
        content.append(&host);
        let port = gtk::Entry::new();
        port.set_text("993");
        port.set_placeholder_text(Some("IMAP port (993 implicit TLS, else STARTTLS)"));
        content.append(&port);
        let username = gtk::Entry::new();
        username.set_placeholder_text(Some("Username"));
        content.append(&username);
        let password = gtk::Entry::new();
        password.set_visibility(false);
        password.set_placeholder_text(Some("Password (kept in memory only, per call)"));
        content.append(&password);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("toolbar");
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("suggested-action");
        let forget = gtk::Button::with_label("Disconnect & forget");
        actions.append(&connect);
        actions.append(&forget);
        content.append(&actions);
        let connection_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        connection_row.append(&spinner);
        let connection_notice = label("Not connected. Enter provider details and press Connect.");
        connection_notice.set_hexpand(true);
        connection_row.append(&connection_notice);
        content.append(&connection_row);
        settings.set_child(Some(&content));
        root.append(&settings);

        let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        folder_row.add_css_class("toolbar");
        let folder_combo = gtk::ComboBoxText::new();
        folder_combo.set_hexpand(true);
        let refresh_folders = gtk::Button::with_label("Refresh folders");
        let refresh_messages = gtk::Button::with_label("Refresh messages");
        folder_row.append(&folder_combo);
        folder_row.append(&refresh_folders);
        folder_row.append(&refresh_messages);
        root.append(&folder_row);
        let list_notice = label("No folder data. Refresh is always explicit; nothing polls.");
        root.append(&list_notice);

        let messages = gtk::ListBox::new();
        messages.set_selection_mode(gtk::SelectionMode::Single);
        let message_scroll = gtk::ScrolledWindow::new();
        message_scroll.set_min_content_height(120);
        message_scroll.set_child(Some(&messages));
        root.append(&message_scroll);

        let read_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        read_row.add_css_class("toolbar");
        let read = gtk::Button::with_label("Read selected");
        read_row.append(&read);
        root.append(&read_row);
        let read_notice = label("Nothing selected.");
        root.append(&read_notice);
        let body_view = gtk::TextView::new();
        body_view.set_editable(false);
        body_view.set_wrap_mode(gtk::WrapMode::WordChar);
        let body_scroll = gtk::ScrolledWindow::new();
        body_scroll.set_min_content_height(120);
        body_scroll.set_child(Some(&body_view));
        root.append(&body_scroll);

        let compose_heading = label("Compose (plain text only, no attachments)");
        compose_heading.add_css_class("section-heading");
        root.append(&compose_heading);
        let to = gtk::Entry::new();
        to.set_placeholder_text(Some("To (one exact address)"));
        root.append(&to);
        let subject = gtk::Entry::new();
        subject.set_placeholder_text(Some("Subject"));
        root.append(&subject);
        let compose_body = gtk::TextView::new();
        compose_body.set_widget_name("mail-compose-body");
        compose_body.set_wrap_mode(gtk::WrapMode::WordChar);
        let compose_scroll = gtk::ScrolledWindow::new();
        compose_scroll.set_min_content_height(100);
        compose_scroll.set_child(Some(&compose_body));
        root.append(&compose_scroll);
        let stage = gtk::Button::with_label("Stage for approval");
        stage.add_css_class("suggested-action");
        root.append(&stage);
        let send_notice =
            label("No draft staged. Nothing is sent without fresh per-send approval.");
        root.append(&send_notice);

        let approval_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        approval_card.add_css_class("card");
        approval_card.add_css_class("approval-card");
        let approval_heading = label("Approve send — exact bytes below go out verbatim");
        approval_heading.add_css_class("approval-heading");
        approval_card.append(&approval_heading);
        let preview = gtk::TextView::new();
        preview.set_editable(false);
        preview.set_wrap_mode(gtk::WrapMode::WordChar);
        let preview_scroll = gtk::ScrolledWindow::new();
        preview_scroll.set_min_content_height(100);
        preview_scroll.set_child(Some(&preview));
        preview_scroll.add_css_class("approval-details");
        approval_card.append(&preview_scroll);
        let approval_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        approval_actions.add_css_class("approval-actions");
        let approve = gtk::Button::with_label("Approve & send once");
        approve.add_css_class("suggested-action");
        let deny = gtk::Button::with_label("Deny");
        deny.add_css_class("destructive-action");
        approval_actions.append(&approve);
        approval_actions.append(&deny);
        approval_card.append(&approval_actions);
        approval_card.set_visible(false);
        root.append(&approval_card);

        let ui = Rc::new(Ui {
            epoch: Cell::new(0),
            busy: Cell::new(false),
            snapshot: RefCell::new(None),
            folders: RefCell::new(Vec::new()),
            envelopes: RefCell::new(Vec::new()),
            controller: RefCell::new(DraftController::new()),
            host,
            port,
            username,
            password,
            connection_notice,
            folder_combo,
            list_notice,
            messages,
            read_notice,
            body_view,
            to,
            subject,
            compose_body,
            send_notice,
            approval_card,
            preview,
            spinner,
        });

        {
            let ui = Rc::clone(&ui);
            connect.connect_clicked(move |_| ui.refresh_folders(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            refresh_folders.connect_clicked(move |_| ui.refresh_folders(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            refresh_messages.connect_clicked(move |_| ui.refresh_messages(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            read.connect_clicked(move |_| ui.read_selected(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            stage.connect_clicked(move |_| ui.stage_draft());
        }
        {
            let ui = Rc::clone(&ui);
            approve.connect_clicked(move |_| ui.approve_staged(&ui));
        }
        {
            let ui = Rc::clone(&ui);
            deny.connect_clicked(move |_| ui.deny_staged());
        }
        {
            let ui = Rc::clone(&ui);
            forget.connect_clicked(move |_| ui.forget());
        }

        MailPage {
            root,
            #[cfg(test)]
            ui,
        }
    }
}

#[cfg(not(mail_controller_only))]
pub use native::{MailPage, build};

#[cfg(test)]
mod tests {
    use super::native::build;
    use gtk::prelude::*;
    use gtk4 as gtk;

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = vec![widget.clone()];
        let mut index = 0;
        while index < out.len() {
            let mut child = out[index].first_child();
            while let Some(next) = child {
                out.push(next.clone().upcast());
                child = next.next_sibling();
            }
            index += 1;
        }
        out
    }

    fn button(root: &gtk::Box, text: &str) -> gtk::Button {
        descendants(root.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some(text))
            .unwrap()
    }

    fn entry(root: &gtk::Box, placeholder: &str) -> gtk::Entry {
        descendants(root.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Entry>().ok())
            .find(|entry| entry.placeholder_text().as_deref() == Some(placeholder))
            .unwrap()
    }

    fn compose_body(root: &gtk::Box) -> gtk::TextView {
        descendants(root.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::TextView>().ok())
            .find(|view| view.widget_name() == "mail-compose-body")
            .unwrap()
    }

    fn has_label(root: &gtk::Box, text: &str) -> bool {
        descendants(root.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
            .any(|label| label.text().contains(text))
    }

    fn text_views(root: &gtk::Box) -> Vec<String> {
        descendants(root.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::TextView>().ok())
            .map(|view| {
                let buffer = view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            })
            .collect()
    }

    #[test]
    #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
    fn approval_card_shows_exact_bytes_and_deny_discards_silently_never() {
        gtk::init().unwrap();
        let page = build();
        entry(&page.root, "To (one exact address)").set_text("nurse@example.com");
        entry(&page.root, "Subject").set_text("Shift notes");
        compose_body(&page.root)
            .buffer()
            .set_text("Cover went fine.");
        button(&page.root, "Stage for approval").emit_clicked();
        assert!(has_label(&page.root, "Approving sends once"));
        let views = text_views(&page.root);
        assert!(
            views
                .iter()
                .any(|text| text.contains("nurse@example.com") && text.contains("Shift notes")),
            "approval preview must show the exact staged bytes"
        );
        button(&page.root, "Deny").emit_clicked();
        assert!(has_label(&page.root, "Nothing was sent"));
    }

    #[test]
    #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
    fn forget_refuses_while_a_request_is_running() {
        // Regression: forget() bumped the epoch and reported teardown while
        // an in-flight send worker could still complete its SMTP
        // transaction unnoticed. Now it refuses while busy.
        gtk::init().unwrap();
        let page = build();
        page.ui.busy.set(true);
        button(&page.root, "Disconnect & forget").emit_clicked();
        assert!(has_label(&page.root, "already running"));
        assert!(page.ui.busy.get(), "busy work must not be cancelled");
        page.ui.busy.set(false);
        button(&page.root, "Disconnect & forget").emit_clicked();
        assert!(has_label(&page.root, "Password cleared"));
    }
}
