use gtk::prelude::*;
use gtk4 as gtk;
mod account_login;
mod account_ui;
mod local_host;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatRole {
    User,
    Assistant,
    Notice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BubbleAlignment {
    Start,
    End,
}

fn bubble_alignment(role: ChatRole) -> BubbleAlignment {
    match role {
        ChatRole::User => BubbleAlignment::End,
        ChatRole::Assistant | ChatRole::Notice => BubbleAlignment::Start,
    }
}

const DARK_THEME_CSS: &str = r#"
window, .app-shell { background-color: #111318; color: #e9ebf0; }
.navigation-rail { background-color: #151820; border-right: 1px solid #292d37; padding: 14px 10px; }
.brand-mark { background-color: #2867c7; color: #ffffff; border-radius: 16px; font-weight: 700; min-width: 42px; min-height: 42px; }
.rail-button { min-width: 44px; min-height: 44px; padding: 0; border-radius: 14px; background: transparent; color: #b5bac6; }
.rail-button:hover { background-color: #242833; color: #ffffff; }
.rail-button.active { background-color: #26344a; color: #8fbdff; }
.main-column { background-color: #111318; }
.topbar { padding: 12px 20px; border-bottom: 1px solid #292d37; }
.page-title { background-color: #1d2028; border-radius: 20px; padding: 8px 14px; font-weight: 600; }
.status-chip { color: #aeb5c2; background-color: #1b1e25; border: 1px solid #2b303b; border-radius: 14px; padding: 6px 10px; }
.page-content { padding: 18px 24px; }
.assistant-identity { padding: 0 0 6px; }
.assistant-avatar { background-color: #303947; color: #dbe9ff; border-radius: 20px; min-width: 40px; min-height: 40px; font-weight: 700; }
.assistant-name { font-weight: 600; }
.settings-disclosure { background-color: #191c23; border: 1px solid #292d37; border-radius: 14px; padding: 10px 14px; }
.settings-content { padding: 10px 2px 4px; }
.chat-bubble { padding: 12px 16px; border-radius: 20px; }
.chat-bubble.user { background-color: #173d67; color: #edf5ff; }
.chat-bubble.assistant { background-color: #222630; color: #edf0f5; }
.chat-bubble.notice { background-color: #352d1d; color: #f4d99b; }
.chat-status { color: #aeb5c2; padding: 2px 6px; }
.composer { background-color: #1c2028; border: 1px solid #303541; border-radius: 22px; padding: 8px; }
.composer entry { background: transparent; border: 0; box-shadow: none; }
entry, searchentry, textview { color: #edf0f5; background-color: #191c23; border: 1px solid #303541; border-radius: 12px; }
button { color: #e9ebf0; background-color: #242832; border: 1px solid #303541; border-radius: 12px; padding: 8px 12px; }
button:hover { background-color: #303541; }
button.suggested-action { color: #ffffff; background-color: #2867c7; border-color: #2867c7; }
button.suggested-action:hover { background-color: #3478dc; }
.dim-label { color: #aeb5c2; }
.card { color: #c6cad3; background-color: #1b1e25; border: 1px solid #292d37; border-radius: 12px; padding: 12px; }
checkbutton { color: #d7dae1; }
"#;

fn install_dark_theme() {
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(DARK_THEME_CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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

/// Default window size. Kept inside a 1366x768 laptop work area: a larger
/// default opened partially off-screen and forced a maximize cycle.
/// Bounds are enforced at compile time on every build, not just tests.
const DEFAULT_WINDOW_WIDTH: i32 = 1100;
const DEFAULT_WINDOW_HEIGHT: i32 = 700;
const DEFAULT_WINDOW_SIZE_PIN: () = {
    assert!(DEFAULT_WINDOW_WIDTH >= 900);
    assert!(DEFAULT_WINDOW_WIDTH <= 1150);
    assert!(DEFAULT_WINDOW_HEIGHT >= 600);
    assert!(DEFAULT_WINDOW_HEIGHT <= 750);
};

fn build_ui(app: &gtk::Application) {
    const _: () = DEFAULT_WINDOW_SIZE_PIN;
    install_dark_theme();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Unofficial Muse for Linux")
        .default_width(DEFAULT_WINDOW_WIDTH)
        .default_height(DEFAULT_WINDOW_HEIGHT)
        .build();
    window.add_css_class("dark-window");

    let grant: SharedGrant = Rc::new(RefCell::new(None));
    let current_directory: SharedDirectory = Rc::new(RefCell::new(PathBuf::new()));
    let active_dialog = DialogLease::<gtk::FileChooserNative>::default();
    let api_key: SharedApiKey = Rc::new(RefCell::new(None));
    let session: SharedSession = Rc::new(RefCell::new(AgentSession::new()));
    let agent_busy = Rc::new(Cell::new(false));

    let shell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    shell.add_css_class("app-shell");

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar.add_css_class("navigation-rail");
    sidebar.set_size_request(68, -1);
    let brand = gtk::Label::new(Some("M"));
    brand.add_css_class("brand-mark");
    sidebar.append(&brand);
    let chats_nav = gtk::Button::from_icon_name("chat-symbolic");
    chats_nav.set_tooltip_text(Some("Chats"));
    chats_nav.add_css_class("rail-button");
    chats_nav.add_css_class("active");
    sidebar.append(&chats_nav);
    let files_nav = gtk::Button::from_icon_name("folder-symbolic");
    files_nav.set_tooltip_text(Some("Files"));
    files_nav.add_css_class("rail-button");
    sidebar.append(&files_nav);
    let host_nav = gtk::Button::from_icon_name("network-server-symbolic");
    host_nav.set_tooltip_text(Some(
        "Local host · consent, folder grants & create approvals",
    ));
    host_nav.add_css_class("rail-button");
    sidebar.append(&host_nav);
    let developer_nav = gtk::Button::from_icon_name("applications-engineering-symbolic");
    developer_nav.set_tooltip_text(Some("Developer experiment — separate paid API key"));
    developer_nav.add_css_class("rail-button");
    sidebar.append(&developer_nav);
    let nav_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_spacer.set_vexpand(true);
    sidebar.append(&nav_spacer);

    let main_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_column.add_css_class("main-column");
    main_column.set_hexpand(true);
    main_column.set_vexpand(true);
    let topbar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    topbar.add_css_class("topbar");
    let page_title = gtk::Label::new(Some("Muse account · Chats & tasks"));
    page_title.add_css_class("page-title");
    topbar.append(&page_title);
    let service_status = gtk::Label::new(Some("Muse account disconnected • local files read-only"));
    service_status.add_css_class("status-chip");
    service_status.set_hexpand(true);
    service_status.set_halign(gtk::Align::End);
    topbar.append(&service_status);
    let settings_button = gtk::Button::from_icon_name("preferences-system-symbolic");
    settings_button.set_tooltip_text(Some("Muse account connection and sign-in"));
    topbar.append(&settings_button);
    main_column.append(&topbar);

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_vhomogeneous(false);
    stack.set_hhomogeneous(false);
    let account_page: account_ui::AccountPage = account_ui::build(&service_status);
    stack.add_named(&account_page.root, Some("chats"));
    let host_page = local_host::build(
        account_page.host_context.clone(),
        account_page.host_device.clone(),
    );
    *account_page.host_boundary.borrow_mut() = Some(host_page.shutdown.clone());
    stack.add_named(&host_page.root, Some("host"));
    let cancel_login = Rc::clone(&account_page.cancel_login);
    let stop_host = host_page.shutdown.clone();
    window.connect_close_request(move |_| {
        stop_host();
        cancel_login();
        gtk::glib::Propagation::Proceed
    });
    stack.set_visible_child_name("chats");
    let chat_page = gtk::Box::new(gtk::Orientation::Vertical, 14);
    chat_page.add_css_class("page-content");

    let assistant_identity = gtk::Box::new(gtk::Orientation::Vertical, 5);
    assistant_identity.set_halign(gtk::Align::Center);
    assistant_identity.add_css_class("assistant-identity");
    let assistant_avatar = gtk::Label::new(Some("M"));
    assistant_avatar.add_css_class("assistant-avatar");
    let assistant_name = gtk::Label::new(Some("Developer experiment · Meta Model API"));
    assistant_name.add_css_class("assistant-name");
    assistant_identity.append(&assistant_avatar);
    assistant_identity.append(&assistant_name);
    chat_page.append(&assistant_identity);

    let setup_expander = gtk::Expander::new(Some("Connection & permissions"));
    setup_expander.add_css_class("settings-disclosure");
    setup_expander.set_expanded(false);
    let setup_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    setup_content.add_css_class("settings-content");
    let disclaimer = gtk::Label::new(Some(
        "Independent, open-source Linux client; not affiliated with Meta. This is a native GTK app, not an embedded Muse website.",
    ));
    disclaimer.set_wrap(true);
    disclaimer.set_xalign(0.0);
    disclaimer.add_css_class("dim-label");
    setup_content.append(&disclaimer);

    let connection = gtk::Label::new(Some(
        "Meta Model API uses a developer key and is separate from consumer Muse sign-in. Prompts are sent only after you submit with paid-use consent enabled; charges may apply.",
    ));
    connection.set_xalign(0.0);
    connection.set_wrap(true);
    connection.add_css_class("card");
    setup_content.append(&connection);

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
    setup_content.append(&credential_row);

    let billing_consent = gtk::CheckButton::with_label(
        "Allow paid Meta Model API requests; prompts leave this device and may be billed.",
    );
    billing_consent.set_halign(gtk::Align::Start);
    setup_content.append(&billing_consent);

    let file_search_consent = gtk::CheckButton::with_label(
        "Allow searching the granted folder; matching file names and paths may be sent to Meta.",
    );
    file_search_consent.set_sensitive(false);
    file_search_consent.set_halign(gtk::Align::Start);
    setup_content.append(&file_search_consent);

    let file_content_consent = gtk::CheckButton::with_label(
        "Allow reading granted text files; up to 16 KiB per read may be sent to Meta.",
    );
    file_content_consent.set_sensitive(false);
    file_content_consent.set_halign(gtk::Align::Start);
    setup_content.append(&file_content_consent);
    setup_expander.set_child(Some(&setup_content));
    chat_page.append(&setup_expander);

    let chat_history = gtk::Box::new(gtk::Orientation::Vertical, 4);
    chat_history.set_valign(gtk::Align::Start);
    append_chat(
        &chat_history,
        ChatRole::Assistant,
        "Developer experiment only. This paid API-key conversation is separate from your personal Muse account and shared history. Use Chats in the sidebar for your existing Muse agent. The key and experimental chat stay in memory for this session.",
    );
    let chat_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .min_content_height(160)
        .build();
    chat_scroll.add_css_class("chat-scroll");
    chat_scroll.set_child(Some(&chat_history));
    chat_page.append(&chat_scroll);

    let chat_status = gtk::Label::new(Some("No API key is active."));
    chat_status.set_xalign(0.0);
    chat_status.set_wrap(true);
    chat_status.add_css_class("chat-status");
    chat_page.append(&chat_status);

    let prompt_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    prompt_row.add_css_class("composer");
    let open_files = gtk::Button::from_icon_name("list-add-symbolic");
    open_files.set_tooltip_text(Some("Open local read-only file browser"));
    prompt_row.append(&open_files);
    let prompt_entry = gtk::Entry::new();
    prompt_entry.set_hexpand(true);
    prompt_entry.set_placeholder_text(Some("Developer experiment: prompt the Meta Model API…"));
    prompt_row.append(&prompt_entry);
    let send_prompt = gtk::Button::with_label("Send");
    send_prompt.add_css_class("suggested-action");
    prompt_row.append(&send_prompt);
    chat_page.append(&prompt_row);

    stack.add_named(&chat_page, Some("developer"));
    main_column.append(&stack);
    shell.append(&sidebar);
    shell.append(&main_column);

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
        clear_chat(&history_for_set);
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
        clear_chat(&history_for_clear);
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
        append_chat(&history_for_send, ChatRole::User, &prompt);
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
                                    clear_chat(&history);
                                    append_chat(&history, ChatRole::Notice, reason);
                                    append_chat(&history, ChatRole::User, &submitted_prompt);
                                }
                                append_chat(&history, ChatRole::Assistant, &response.text);
                                status.set_text("Response complete. Conversation remains in memory until cleared or the app closes.");
                            }
                            Err(error) => {
                                append_chat(&history, ChatRole::Notice, &error);
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

    let files_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    files_page.add_css_class("page-content");
    let files_heading = gtk::Label::new(Some("Local files"));
    files_heading.set_xalign(0.0);
    files_heading.add_css_class("title-2");
    files_page.append(&files_heading);
    let files_explanation = gtk::Label::new(Some(
        "Browse and preview locally. Folder grants are read-only, session-scoped, and separate from sharing consent.",
    ));
    files_explanation.set_xalign(0.0);
    files_explanation.set_wrap(true);
    files_explanation.add_css_class("dim-label");
    files_page.append(&files_explanation);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let choose_folder = gtk::Button::with_label("Grant read access to a folder…");
    choose_folder.add_css_class("suggested-action");
    let location = gtk::Label::new(Some("No folder granted"));
    location.set_xalign(0.0);
    location.set_selectable(true);
    toolbar.append(&choose_folder);
    toolbar.append(&location);
    files_page.append(&toolbar);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search file names in the granted folder")
        .sensitive(false)
        .build();
    files_page.append(&search);

    let status = gtk::Label::new(Some(
        "Choose a folder to browse it. Access is read-only and remains in memory for this session.",
    ));
    status.set_xalign(0.0);
    status.set_wrap(true);
    files_page.append(&status);

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
    files_page.append(&panes);

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

    stack.add_named(&files_page, Some("files"));

    let navigation = [
        (&chats_nav, "chats"),
        (&files_nav, "files"),
        (&host_nav, "host"),
        (&developer_nav, "developer"),
    ];
    for (button, page) in navigation {
        let stack = stack.clone();
        let title = page_title.clone();
        let chats = chats_nav.clone();
        let files = files_nav.clone();
        let host = host_nav.clone();
        let developer = developer_nav.clone();
        button.connect_clicked(move |_| {
            select_page(
                &stack,
                &title,
                &[
                    (&chats, "chats"),
                    (&files, "files"),
                    (&host, "host"),
                    (&developer, "developer"),
                ],
                page,
            );
        });
    }

    let stack_for_open_files = stack.clone();
    let title_for_open_files = page_title.clone();
    let chats_for_open_files = chats_nav.clone();
    let files_for_open_files = files_nav.clone();
    let host_for_open_files = host_nav.clone();
    let developer_for_open_files = developer_nav.clone();
    open_files.connect_clicked(move |_| {
        select_page(
            &stack_for_open_files,
            &title_for_open_files,
            &[
                (&chats_for_open_files, "chats"),
                (&files_for_open_files, "files"),
                (&host_for_open_files, "host"),
                (&developer_for_open_files, "developer"),
            ],
            "files",
        );
    });

    let stack_for_settings = stack.clone();
    let title_for_settings = page_title.clone();
    let chats_for_settings = chats_nav.clone();
    let files_for_settings = files_nav.clone();
    let host_for_settings = host_nav.clone();
    let developer_for_settings = developer_nav.clone();
    let setup_for_settings = account_page.settings.clone();
    settings_button.connect_clicked(move |_| {
        select_page(
            &stack_for_settings,
            &title_for_settings,
            &[
                (&chats_for_settings, "chats"),
                (&files_for_settings, "files"),
                (&host_for_settings, "host"),
                (&developer_for_settings, "developer"),
            ],
            "chats",
        );
        setup_for_settings.set_expanded(true);
    });

    window.set_child(Some(&shell));
    window.present();
}

fn select_page(
    stack: &gtk::Stack,
    title: &gtk::Label,
    navigation: &[(&gtk::Button, &str)],
    page: &str,
) {
    stack.set_visible_child_name(page);
    title.set_text(match page {
        "chats" => "Muse account · Chats & tasks",
        "files" => "Files",
        "host" => "Local host · grants & approvals",
        "developer" => "Developer experiment",
        _ => "Unknown page",
    });
    for (button, name) in navigation {
        button.set_css_classes(if *name == page {
            &["rail-button", "active"]
        } else {
            &["rail-button"]
        });
    }
}

fn append_chat(container: &gtk::Box, role: ChatRole, message: &str) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.set_hexpand(true);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_margin_top(4);
    row.set_margin_bottom(4);

    let bubble = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bubble.set_halign(match bubble_alignment(role) {
        BubbleAlignment::Start => gtk::Align::Start,
        BubbleAlignment::End => gtk::Align::End,
    });
    bubble.add_css_class("chat-bubble");
    bubble.add_css_class(match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Notice => "notice",
    });
    let label = gtk::Label::new(Some(message));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true);
    label.set_max_width_chars(88);
    bubble.append(&label);
    row.append(&bubble);
    container.append(&row);
}

fn clear_chat(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
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
    fn chat_message_roles_place_user_on_right_and_assistant_on_left() {
        assert_eq!(
            super::bubble_alignment(super::ChatRole::User),
            super::BubbleAlignment::End
        );
        assert_eq!(
            super::bubble_alignment(super::ChatRole::Assistant),
            super::BubbleAlignment::Start
        );
        assert_eq!(
            super::bubble_alignment(super::ChatRole::Notice),
            super::BubbleAlignment::Start
        );
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
