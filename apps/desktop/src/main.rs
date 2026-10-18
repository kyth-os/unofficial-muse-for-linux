use gtk::prelude::*;
use gtk4 as gtk;
use muse_agent_runtime::{AgentResponse, AgentSession, ToolPolicy};
use muse_linux_core::filesystem::{FileEntry, FilesystemGrant};
use muse_meta_api::MetaModelClient;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const APP_ID: &str = "org.kythos.UnofficialMuseForLinux";

type SharedGrant = Rc<RefCell<Option<FilesystemGrant>>>;
type SharedDirectory = Rc<RefCell<PathBuf>>;
type SharedApiKey = Rc<RefCell<Option<String>>>;
type SharedSession = Rc<RefCell<AgentSession>>;

enum AgentUiMessage {
    Progress(String),
    Finished(AgentSession, Result<AgentResponse, String>),
}

#[derive(Clone)]
struct DialogLease<T> {
    active: Rc<RefCell<Option<T>>>,
}

impl<T> Default for DialogLease<T> {
    fn default() -> Self {
        Self {
            active: Rc::new(RefCell::new(None)),
        }
    }
}

impl<T> DialogLease<T> {
    fn keep(&self, dialog: T) {
        *self.active.borrow_mut() = Some(dialog);
    }

    fn is_active(&self) -> bool {
        self.active.borrow().is_some()
    }

    fn weak(&self) -> std::rc::Weak<RefCell<Option<T>>> {
        Rc::downgrade(&self.active)
    }
}

fn main() {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Unofficial Muse for Linux")
        .default_width(1080)
        .default_height(720)
        .build();

    let grant: SharedGrant = Rc::new(RefCell::new(None));
    let current_directory: SharedDirectory = Rc::new(RefCell::new(PathBuf::new()));
    let active_dialog = DialogLease::<gtk::FileChooserNative>::default();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    root.set_margin_start(24);
    root.set_margin_end(24);

    let title = gtk::Label::new(Some("Muse for Linux"));
    title.set_xalign(0.0);
    title.add_css_class("title-1");
    root.append(&title);

    let disclaimer = gtk::Label::new(Some(
        "Unofficial, open-source client. Not affiliated with Meta. This early build uses a custom Linux UI and local integrations; it does not embed the Muse website.",
    ));
    disclaimer.set_wrap(true);
    disclaimer.set_xalign(0.0);
    disclaimer.add_css_class("dim-label");
    root.append(&disclaimer);

    let connection = gtk::Label::new(Some(
        "Meta Model API uses a developer key and is separate from consumer Muse sign-in. Prompts are sent only after you submit with paid-use consent enabled; charges may apply.",
    ));
    connection.set_xalign(0.0);
    connection.set_wrap(true);
    connection.add_css_class("card");
    root.append(&connection);

    let api_key: SharedApiKey = Rc::new(RefCell::new(None));
    let session: SharedSession = Rc::new(RefCell::new(AgentSession::new()));
    let agent_busy = Rc::new(Cell::new(false));

    let credential_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let key_entry = gtk::Entry::new();
    key_entry.set_visibility(false);
    key_entry.set_hexpand(true);
    key_entry.set_placeholder_text(Some("Meta Model API key (kept in memory only)"));
    let use_key = gtk::Button::with_label("Use key this session");
    let clear_session = gtk::Button::with_label("Forget key & chat");
    credential_row.append(&key_entry);
    credential_row.append(&use_key);
    credential_row.append(&clear_session);
    root.append(&credential_row);

    let billing_consent = gtk::CheckButton::with_label(
        "Allow paid Meta Model API requests; prompts leave this device and may be billed.",
    );
    billing_consent.set_halign(gtk::Align::Start);
    root.append(&billing_consent);

    let file_search_consent = gtk::CheckButton::with_label(
        "Allow searching the granted folder; matching file names and paths may be sent to Meta.",
    );
    file_search_consent.set_sensitive(false);
    file_search_consent.set_halign(gtk::Align::Start);
    root.append(&file_search_consent);

    let file_content_consent = gtk::CheckButton::with_label(
        "Allow reading granted text files; up to 16 KiB per read may be sent to Meta.",
    );
    file_content_consent.set_sensitive(false);
    file_content_consent.set_halign(gtk::Align::Start);
    root.append(&file_content_consent);

    let chat_history = gtk::TextView::new();
    chat_history.set_editable(false);
    chat_history.set_cursor_visible(false);
    chat_history.set_wrap_mode(gtk::WrapMode::WordChar);
    chat_history
        .buffer()
        .set_text("Assistant: Enter a Meta Model API developer key to start. The key and conversation are session-only.");
    let chat_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .min_content_height(130)
        .build();
    chat_scroll.set_child(Some(&chat_history));
    root.append(&chat_scroll);

    let chat_status = gtk::Label::new(Some("No API key is active."));
    chat_status.set_xalign(0.0);
    chat_status.set_wrap(true);
    chat_status.add_css_class("dim-label");
    root.append(&chat_status);

    let prompt_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let prompt_entry = gtk::Entry::new();
    prompt_entry.set_hexpand(true);
    prompt_entry.set_placeholder_text(Some("Ask Muse…"));
    let send_prompt = gtk::Button::with_label("Send");
    send_prompt.add_css_class("suggested-action");
    prompt_row.append(&prompt_entry);
    prompt_row.append(&send_prompt);
    root.append(&prompt_row);

    let api_key_for_set = Rc::clone(&api_key);
    let session_for_set = Rc::clone(&session);
    let history_for_set = chat_history.clone();
    let status_for_set = chat_status.clone();
    let entry_for_set = key_entry.clone();
    let billing_for_set = billing_consent.clone();
    let search_consent_for_set = file_search_consent.clone();
    let content_consent_for_set = file_content_consent.clone();
    use_key.connect_clicked(move |_| {
        let key = entry_for_set.text().to_string();
        if key.trim().is_empty() {
            status_for_set.set_text("Enter a developer key. It is held only in memory and is never saved to disk.");
            return;
        }
        *api_key_for_set.borrow_mut() = Some(key);
        *session_for_set.borrow_mut() = AgentSession::new();
        billing_for_set.set_active(false);
        search_consent_for_set.set_active(false);
        content_consent_for_set.set_active(false);
        history_for_set.buffer().set_text("");
        entry_for_set.set_text("");
        status_for_set.set_text("Key held in memory only. No request is made until you send a prompt with paid-use consent enabled.");
    });

    let api_key_for_clear = Rc::clone(&api_key);
    let session_for_clear = Rc::clone(&session);
    let history_for_clear = chat_history.clone();
    let status_for_clear = chat_status.clone();
    let entry_for_clear = key_entry.clone();
    let billing_for_clear = billing_consent.clone();
    let search_consent_for_clear = file_search_consent.clone();
    let content_consent_for_clear = file_content_consent.clone();
    clear_session.connect_clicked(move |_| {
        api_key_for_clear.borrow_mut().take();
        *session_for_clear.borrow_mut() = AgentSession::new();
        billing_for_clear.set_active(false);
        search_consent_for_clear.set_active(false);
        content_consent_for_clear.set_active(false);
        history_for_clear.buffer().set_text("");
        entry_for_clear.set_text("");
        status_for_clear.set_text("API key and conversation cleared from this session.");
    });

    let api_key_for_send = Rc::clone(&api_key);
    let session_for_send = Rc::clone(&session);
    let busy_for_send = Rc::clone(&agent_busy);
    let grant_for_send = Rc::clone(&grant);
    let billing_for_send = billing_consent.clone();
    let search_consent_for_send = file_search_consent.clone();
    let content_consent_for_send = file_content_consent.clone();
    let prompt_for_send = prompt_entry.clone();
    let history_for_send = chat_history.clone();
    let chat_status_for_send = chat_status.clone();
    let send_button_for_send = send_prompt.clone();
    let use_key_for_send = use_key.clone();
    let clear_session_for_send = clear_session.clone();
    let billing_control_for_send = billing_consent.clone();
    let search_control_for_send = file_search_consent.clone();
    let content_control_for_send = file_content_consent.clone();
    let grant_for_ui = Rc::clone(&grant_for_send);
    send_prompt.connect_clicked(move |_| {
        if busy_for_send.get() {
            return;
        }
        let prompt = prompt_for_send.text().to_string();
        if prompt.trim().is_empty() {
            chat_status_for_send.set_text("Enter a prompt first.");
            return;
        }
        let Some(api_key) = api_key_for_send.borrow().clone() else {
            chat_status_for_send.set_text("Enter and activate a Meta Model API developer key first.");
            return;
        };
        if !billing_for_send.is_active() {
            chat_status_for_send.set_text("Enable paid-use consent before sending. No request was made.");
            return;
        }

        let file_grant = grant_for_send.borrow().clone();
        let policy = ToolPolicy {
            search_names: search_consent_for_send.is_active(),
            read_file_contents: content_consent_for_send.is_active(),
        };
        let mut worker_session = session_for_send.borrow().clone();
        append_chat(&history_for_send.buffer(), "You", &prompt);
        prompt_for_send.set_text("");
        busy_for_send.set(true);
        prompt_for_send.set_sensitive(false);
        send_button_for_send.set_sensitive(false);
        use_key_for_send.set_sensitive(false);
        clear_session_for_send.set_sensitive(false);
        billing_control_for_send.set_sensitive(false);
        search_control_for_send.set_sensitive(false);
        content_control_for_send.set_sensitive(false);
        chat_status_for_send.set_text("Sending prompt to Meta Model API…");

        let (sender, receiver) = mpsc::channel();
        let progress_sender = sender.clone();
        let submitted_prompt = prompt.clone();
        std::thread::spawn(move || {
            let result = match MetaModelClient::new(api_key) {
                Ok(client) => worker_session
                    .run_turn(
                        &client,
                        &prompt,
                        file_grant.as_ref(),
                        policy,
                        |event| {
                            let _ = progress_sender.send(AgentUiMessage::Progress(format!(
                                "{}: {}",
                                event.tool_name, event.summary
                            )));
                        },
                    )
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(AgentUiMessage::Finished(worker_session, result));
        });

        let session_state = Rc::clone(&session_for_send);
        let busy_state = Rc::clone(&busy_for_send);
        let prompt_entry = prompt_for_send.clone();
        let send_button = send_button_for_send.clone();
        let history = history_for_send.clone();
        let status = chat_status_for_send.clone();
        let use_key_button = use_key_for_send.clone();
        let clear_session_button = clear_session_for_send.clone();
        let billing_control = billing_control_for_send.clone();
        let search_control = search_control_for_send.clone();
        let content_control = content_control_for_send.clone();
        let grant_state = Rc::clone(&grant_for_ui);
        gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            let mut finished = false;
            loop {
                match receiver.try_recv() {
                    Ok(AgentUiMessage::Progress(progress)) => status.set_text(&progress),
                    Ok(AgentUiMessage::Finished(updated_session, result)) => {
                        *session_state.borrow_mut() = updated_session;
                        match result {
                            Ok(response) => {
                                if let Some(reason) = response.context_reset.as_deref() {
                                    history.buffer().set_text("");
                                    append_chat(&history.buffer(), "Notice", reason);
                                    append_chat(&history.buffer(), "You", &submitted_prompt);
                                }
                                append_chat(&history.buffer(), "Muse", &response.text);
                                status.set_text("Response complete. Conversation remains in memory until cleared or the app closes.");
                            }
                            Err(error) => {
                                append_chat(&history.buffer(), "Request failed", &error);
                                status.set_text("Request failed. The API key and chat remain session-only.");
                            }
                        }
                        busy_state.set(false);
                        prompt_entry.set_sensitive(true);
                        send_button.set_sensitive(true);
                        use_key_button.set_sensitive(true);
                        clear_session_button.set_sensitive(true);
                        billing_control.set_sensitive(true);
                        let has_grant = grant_state.borrow().is_some();
                        search_control.set_sensitive(has_grant);
                        content_control.set_sensitive(has_grant);
                        finished = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        status.set_text("Agent worker stopped unexpectedly.");
                        busy_state.set(false);
                        prompt_entry.set_sensitive(true);
                        send_button.set_sensitive(true);
                        use_key_button.set_sensitive(true);
                        clear_session_button.set_sensitive(true);
                        billing_control.set_sensitive(true);
                        let has_grant = grant_state.borrow().is_some();
                        search_control.set_sensitive(has_grant);
                        content_control.set_sensitive(has_grant);
                        finished = true;
                        break;
                    }
                }
            }
            if finished {
                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });
    });

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let choose_folder = gtk::Button::with_label("Grant read access to a folder…");
    choose_folder.add_css_class("suggested-action");
    let location = gtk::Label::new(Some("No folder granted"));
    location.set_xalign(0.0);
    location.set_selectable(true);
    toolbar.append(&choose_folder);
    toolbar.append(&location);
    root.append(&toolbar);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search file names in the granted folder")
        .sensitive(false)
        .build();
    root.append(&search);

    let status = gtk::Label::new(Some(
        "Choose a folder to browse it. Access is read-only and remains in memory for this session.",
    ));
    status.set_xalign(0.0);
    status.set_wrap(true);
    root.append(&status);

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    panes.set_vexpand(true);

    let file_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .min_content_width(340)
        .build();
    let file_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    file_list.set_margin_top(8);
    file_list.set_margin_bottom(8);
    file_list.set_margin_start(8);
    file_list.set_margin_end(8);
    file_scroll.set_child(Some(&file_list));

    let preview_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .min_content_width(420)
        .build();
    let preview = gtk::TextView::new();
    preview.set_editable(false);
    preview.set_cursor_visible(false);
    preview.set_monospace(true);
    preview.set_wrap_mode(gtk::WrapMode::WordChar);
    preview
        .buffer()
        .set_text("Select a text file to preview it here.");
    preview_scroll.set_child(Some(&preview));

    panes.append(&file_scroll);
    panes.append(&preview_scroll);
    root.append(&panes);

    let grant_for_dialog = Rc::clone(&grant);
    let directory_for_dialog = Rc::clone(&current_directory);
    let status_for_dialog = status.clone();
    let location_for_dialog = location.clone();
    let list_for_dialog = file_list.clone();
    let preview_for_dialog = preview.clone();
    let search_for_dialog = search.clone();
    let search_consent_for_dialog = file_search_consent.clone();
    let content_consent_for_dialog = file_content_consent.clone();
    let parent_for_dialog = window.clone();
    let active_dialog_for_click = active_dialog.clone();
    let busy_for_dialog = Rc::clone(&agent_busy);
    choose_folder.connect_clicked(move |_| {
        if busy_for_dialog.get() {
            status_for_dialog
                .set_text("Wait for the current assistant task before changing the folder grant.");
            return;
        }
        if active_dialog_for_click.is_active() {
            return;
        }

        let dialog = gtk::FileChooserNative::new(
            Some("Choose a folder to grant read access"),
            Some(&parent_for_dialog),
            gtk::FileChooserAction::SelectFolder,
            Some("Grant read access"),
            Some("Cancel"),
        );

        let grant_state = Rc::clone(&grant_for_dialog);
        let directory_state = Rc::clone(&directory_for_dialog);
        let status_label = status_for_dialog.clone();
        let location_label = location_for_dialog.clone();
        let list = list_for_dialog.clone();
        let preview = preview_for_dialog.clone();
        let search_entry = search_for_dialog.clone();
        let search_consent = search_consent_for_dialog.clone();
        let content_consent = content_consent_for_dialog.clone();
        let weak_dialog_lease = active_dialog_for_click.weak();
        dialog.connect_response(move |chooser, response| {
            if response == gtk::ResponseType::Accept {
                match chooser.file().and_then(|file| file.path()) {
                    Some(path) => match FilesystemGrant::new(&path) {
                        Ok(new_grant) => {
                            location_label.set_text(&path.display().to_string());
                            *grant_state.borrow_mut() = Some(new_grant);
                            *directory_state.borrow_mut() = PathBuf::new();
                            search_entry.set_sensitive(true);
                            search_entry.set_text("");
                            search_consent.set_active(false);
                            search_consent.set_sensitive(true);
                            content_consent.set_active(false);
                            content_consent.set_sensitive(true);
                            preview
                                .buffer()
                                .set_text("Select a text file to preview it here.");
                            render_directory(
                                &list,
                                &preview,
                                &status_label,
                                &grant_state,
                                &directory_state,
                            );
                        }
                        Err(error) => {
                            status_label.set_text(&format!("Could not grant folder: {error}"))
                        }
                    },
                    None => status_label
                        .set_text("The selected location is not a local filesystem path."),
                }
            }
            chooser.hide();
            if let Some(active) = weak_dialog_lease.upgrade() {
                active.borrow_mut().take();
            }
        });
        active_dialog_for_click.keep(dialog.clone());
        dialog.show();
    });

    let list_for_search = file_list.clone();
    let preview_for_search = preview.clone();
    let status_for_search = status.clone();
    let grant_for_search = Rc::clone(&grant);
    let directory_for_search = Rc::clone(&current_directory);
    search.connect_search_changed(move |entry| {
        let query = entry.text();
        if query.trim().is_empty() {
            render_directory(
                &list_for_search,
                &preview_for_search,
                &status_for_search,
                &grant_for_search,
                &directory_for_search,
            );
        } else {
            render_search_results(
                &list_for_search,
                &preview_for_search,
                &status_for_search,
                &grant_for_search,
                &directory_for_search,
                query.as_str(),
            );
        }
    });

    window.set_child(Some(&root));
    window.present();
}

fn append_chat(buffer: &gtk::TextBuffer, speaker: &str, message: &str) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("{speaker}: {message}\n\n"));
}

fn render_search_results(
    list: &gtk::Box,
    preview: &gtk::TextView,
    status: &gtk::Label,
    grant: &SharedGrant,
    current_directory: &SharedDirectory,
    query: &str,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    preview
        .buffer()
        .set_text("Select a matching text file to preview it here.");

    let grant_borrow = grant.borrow();
    let Some(granted_root) = grant_borrow.as_ref() else {
        status.set_text("Choose a folder before searching.");
        return;
    };

    match granted_root.search_names(query, 100) {
        Ok(result) => {
            let truncated = result.truncated;
            let files: Vec<FileEntry> = result
                .entries
                .into_iter()
                .filter(|entry| !entry.is_directory)
                .collect();
            if truncated {
                status.set_text(&format!(
                    "At least {} file-name matches for “{query}”; search limits were reached. Results are scoped to the granted folder.",
                    files.len()
                ));
            } else {
                status.set_text(&format!(
                    "{} file-name match{} for “{query}” in the granted folder.",
                    files.len(),
                    if files.len() == 1 { "" } else { "es" }
                ));
            }
            if files.is_empty() {
                list.append(&gtk::Label::new(Some("No matching files found.")));
            } else {
                for entry in files {
                    append_entry(list, preview, status, grant, current_directory, entry);
                }
            }
        }
        Err(error) => status.set_text(&format!("Could not search this folder: {error}")),
    }
}

fn render_directory(
    list: &gtk::Box,
    preview: &gtk::TextView,
    status: &gtk::Label,
    grant: &SharedGrant,
    current_directory: &SharedDirectory,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    let grant_borrow = grant.borrow();
    let Some(granted_root) = grant_borrow.as_ref() else {
        status.set_text("Choose a folder to browse it.");
        return;
    };

    let relative_directory = current_directory.borrow().clone();
    if !relative_directory.as_os_str().is_empty() {
        let up_button = gtk::Button::with_label("← Parent folder");
        let list_again = list.clone();
        let preview_again = preview.clone();
        let status_again = status.clone();
        let grant_again = Rc::clone(grant);
        let directory_again = Rc::clone(current_directory);
        up_button.connect_clicked(move |_| {
            directory_again.borrow_mut().pop();
            render_directory(
                &list_again,
                &preview_again,
                &status_again,
                &grant_again,
                &directory_again,
            );
        });
        list.append(&up_button);
    }

    match granted_root.list(&relative_directory) {
        Ok(entries) => {
            status.set_text(&format!(
                "Read-only grant active • {} entr{} • browsing stays local; agent sharing is separately opt-in.",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" }
            ));
            for entry in entries {
                append_entry(list, preview, status, grant, current_directory, entry);
            }
        }
        Err(error) => status.set_text(&format!("Could not list this folder: {error}")),
    }
}

fn append_entry(
    list: &gtk::Box,
    preview: &gtk::TextView,
    status: &gtk::Label,
    grant: &SharedGrant,
    current_directory: &SharedDirectory,
    entry: FileEntry,
) {
    let label = if entry.is_directory {
        format!("▸  {}/", entry.name)
    } else {
        format!("▤  {}  ·  {} bytes", entry.name, entry.size_bytes)
    };
    let button = gtk::Button::with_label(&label);
    button.set_halign(gtk::Align::Fill);

    if entry.is_directory {
        let path = entry.relative_path;
        let list_again = list.clone();
        let preview_again = preview.clone();
        let status_again = status.clone();
        let grant_again = Rc::clone(grant);
        let directory_again = Rc::clone(current_directory);
        button.connect_clicked(move |_| {
            *directory_again.borrow_mut() = path.clone();
            render_directory(
                &list_again,
                &preview_again,
                &status_again,
                &grant_again,
                &directory_again,
            );
        });
    } else {
        let path = entry.relative_path;
        let status_label = status.clone();
        let preview_widget = preview.clone();
        let grant_state = Rc::clone(grant);
        button.connect_clicked(move |_| {
            let grant_borrow = grant_state.borrow();
            let Some(granted_root) = grant_borrow.as_ref() else {
                status_label.set_text("Choose a folder before reading files.");
                return;
            };
            match granted_root.read_text(&path) {
                Ok(text) => {
                    preview_widget.buffer().set_text(&text);
                    status_label.set_text(&format!(
                        "Previewed {} locally; agent sharing requires separate consent.",
                        path.display()
                    ));
                }
                Err(error) => {
                    preview_widget.buffer().set_text("");
                    status_label.set_text(&format!("Could not read file: {error}"));
                }
            }
        });
    }
    list.append(&button);
}

#[cfg(test)]
mod tests {
    use super::DialogLease;
    use std::cell::Cell;
    use std::rc::Rc;

    struct DropProbe(Rc<Cell<usize>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn dialog_lease_keeps_dialog_alive_until_response_release() {
        let lease = DialogLease::<DropProbe>::default();
        let drops = Rc::new(Cell::new(0));
        lease.keep(DropProbe(Rc::clone(&drops)));

        assert!(lease.is_active());
        assert_eq!(drops.get(), 0);

        if let Some(active) = lease.weak().upgrade() {
            active.borrow_mut().take();
        }

        assert!(!lease.is_active());
        assert_eq!(drops.get(), 1);
    }
}
