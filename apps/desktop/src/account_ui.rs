//! Native account UI. The controller can be tested without GTK or an account.
#![allow(unexpected_cfgs)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Connect,
    Refresh,
    History,
    Send,
    Login,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ticket {
    epoch: u64,
    serial: u64,
    scope: Option<String>,
    limit: usize,
    operation: Operation,
}

struct Controller<A, H, T> {
    epoch: u64,
    serial: u64,
    busy: Option<Ticket>,
    account: Option<A>,
    history: Vec<H>,
    tasks: Option<T>,
    scope: Option<String>,
    limit: usize,
    connection_notice: String,
    history_notice: String,
    tasks_notice: String,
    send_notice: String,
    pending_send: Option<PendingSend>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSend {
    scope: Option<String>,
    text: String,
}

enum SendOutcome {
    Confirmed,
    NotSent(String),
    Uncertain(String),
}

pub(crate) fn login_error_message(code: &str) -> &'static str {
    match code {
        "invalid_request" => "Sign-in helper rejected the selected account directory.",
        "missing_browser" | "browser_not_found" | "browser_unavailable" => {
            "No supported Chromium-compatible browser was available."
        }
        "missing_dependency" => {
            "The selected Python could not run the isolated browser sign-in helper."
        }
        "dependency_missing" | "sdk_unavailable" => {
            "The selected Python is missing the community Muse account or browser dependency."
        }
        "browser_launch_failed" => "The isolated browser could not be started.",
        "unsafe_devtools" => "The isolated browser debugging connection could not be verified.",
        "devtools_error" => "The isolated browser connection failed before sign-in completed.",
        "timeout" | "login_timeout" => "Browser sign-in expired before authentication completed.",
        "unsafe_config" | "insecure_config" => {
            "Choose a private account directory with safe ownership and permissions, without unrelated credentials."
        }
        "cancelled" | "browser_closed" => {
            "Browser sign-in was cancelled or the isolated browser was closed."
        }
        "cleanup_failed" => {
            "Close the dedicated sign-in browser before trying again; browser cleanup could not be confirmed."
        }
        _ => {
            "The sign-in helper reported failure. Check the selected interpreter, isolated browser and account directory; raw output is hidden to protect credentials."
        }
    }
}

impl<A, H, T> Default for Controller<A, H, T> {
    fn default() -> Self {
        Self {
            epoch: 0,
            serial: 0,
            busy: None,
            account: None,
            history: Vec::new(),
            tasks: None,
            scope: None,
            limit: 50,
            connection_notice:
                "Not connected. Select a session and explicitly consent before connecting.".into(),
            history_notice: "No shared history requested.".into(),
            tasks_notice: "No scheduled-task history requested.".into(),
            send_notice: String::new(),
            pending_send: None,
        }
    }
}

impl<A, H, T> Controller<A, H, T> {
    fn begin_login(&mut self, consent: bool) -> Option<Ticket> {
        if !consent || self.account.is_some() {
            self.connection_notice = "Consent to session access before opening sign-in. Disconnect first if already linked.".into();
            return None;
        }
        let ticket = self.issue(Operation::Login)?;
        self.connection_notice = "Sign in at official muse.ai in the isolated Chromium-compatible browser. No password or cookie is shown to this app.".into();
        Some(ticket)
    }

    fn finish_login(&mut self, ticket: &Ticket, result: Result<(), String>) -> bool {
        if !self.accept(ticket) {
            return false;
        }
        self.connection_notice = match result {
            Ok(()) => {
                "Browser sign-in completed. Click Connect to explicitly link this selected session."
                    .into()
            }
            Err(error) => format!("Browser sign-in failed: {error}"),
        };
        true
    }

    fn begin_send(&mut self, text: &str) -> Option<Ticket> {
        self.account.as_ref()?;
        if self.pending_send.is_some() || text.trim().is_empty() {
            return None;
        }
        let ticket = self.issue(Operation::Send)?;
        self.pending_send = Some(PendingSend {
            scope: self.scope.clone(),
            text: text.into(),
        });
        self.send_notice = "Sending once to your existing Muse agent. This can cause external actions; no automatic retry.".into();
        Some(ticket)
    }

    /// Explains a refused send so the UI never fails silently. Every
    /// `begin_send` refusal maps to one of these actionable notices.
    fn describe_send_block(&self, text: &str) -> &'static str {
        if self.account.is_none() {
            return "Not connected. Connect your Muse session before sending.";
        }
        if self.busy.is_some() {
            return "A request is already running. Wait for it to finish before sending.";
        }
        if self.pending_send.is_some() {
            return "A previous send is uncertain. Check its original shared history, then allow another send.";
        }
        if text.trim().is_empty() {
            return "Enter a message before sending.";
        }
        "Send is not ready. Wait for the current request to finish, then try again."
    }

    fn finish_send(&mut self, ticket: &Ticket, outcome: SendOutcome) -> bool {
        if !self.accept(ticket) {
            return false;
        }
        match outcome {
            SendOutcome::Confirmed => {
                self.pending_send = None;
                self.send_notice = "Prompt persistence confirmed in this conversation. This does not mean the agent's work is complete.".into();
            }
            SendOutcome::NotSent(error) => {
                self.pending_send = None;
                self.send_notice =
                    format!("Not sent: {error}. No automatic retry; the composer is unchanged.");
            }
            SendOutcome::Uncertain(error) => {
                let scope = ticket.scope.as_deref().unwrap_or("Main conversation");
                self.send_notice = format!(
                    "Delivery uncertain in {scope}: {error}. It may already be running. Do not repeat it; inspect this conversation's shared history before allowing another send. Original text and scope remain in memory."
                );
            }
        }
        true
    }

    fn acknowledge_send(&mut self) {
        if self.busy.is_none() && self.pending_send.is_some() {
            self.pending_send = None;
            self.send_notice = "You acknowledged the uncertain dispatch after checking its original shared conversation. No resend was performed.".into();
        }
    }

    fn finish_refresh(
        &mut self,
        ticket: &Ticket,
        account: Result<A, String>,
        history: Result<Vec<H>, String>,
        tasks: Result<T, String>,
    ) -> bool {
        if !self.accept(ticket) {
            return false;
        }
        match account {
            Ok(account) => {
                self.account = Some(account);
                self.connection_notice = "Account linked • shared conversations and tasks • refresh every 30 seconds while idle.".into();
            }
            Err(error) => {
                self.connection_notice = format!(
                    "Account refresh failed: {error}. Displayed account details are from the last successful read; disconnect to change sessions."
                )
            }
        }
        self.apply_history(history);
        match tasks {
            Ok(tasks) => {
                self.tasks = Some(tasks);
                self.tasks_notice = "Scheduled tasks and recent runs loaded from Muse (read-only, service-bounded list).".into();
            }
            Err(error) => {
                self.tasks_notice = format!(
                    "Scheduled-task history failed: {error}. Any displayed tasks are from the previous successful read."
                )
            }
        }
        true
    }

    fn select_thread(&mut self, scope: Option<String>) -> bool {
        if self.account.is_none()
            || self.scope == scope
            || self.busy.as_ref().is_some_and(|t| {
                matches!(
                    t.operation,
                    Operation::Send | Operation::Login | Operation::Connect
                )
            })
        {
            return false;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.busy = None;
        self.scope = scope;
        self.history.clear();
        self.limit = 50;
        self.history_notice = "Selected conversation. History has not loaded yet.".into();
        true
    }

    fn begin_read(&mut self, older: bool, refresh: bool) -> Option<Ticket> {
        self.account.as_ref()?;
        if self.busy.is_some() || (older && self.limit >= 200) {
            return None;
        }
        if older {
            self.limit = (self.limit + 50).min(200);
        }
        self.issue(if refresh {
            Operation::Refresh
        } else {
            Operation::History
        })
    }

    fn finish_history(&mut self, ticket: &Ticket, result: Result<Vec<H>, String>) -> bool {
        if !self.accept(ticket) {
            return false;
        }
        self.apply_history(result);
        true
    }

    fn apply_history(&mut self, result: Result<Vec<H>, String>) {
        match result {
            Ok(history) => {
                self.history_notice = format!(
                    "{} messages returned from the latest {} service events (bounded window, not a full export).",
                    history.len(),
                    self.limit
                );
                self.history = history;
            }
            Err(error) => {
                self.history_notice = format!(
                    "Shared history failed: {error}. Any displayed messages are from the previous successful read."
                )
            }
        }
    }

    fn finish_connect(&mut self, ticket: &Ticket, result: Result<A, String>) -> bool {
        if !self.accept(ticket) {
            return false;
        }
        match result {
            Ok(account) => {
                self.account = Some(account);
                self.connection_notice =
                    "Account linked. Shared cloud history; not official Mac-device registration."
                        .into();
            }
            Err(error) => self.connection_notice = format!("Connection failed: {error}"),
        }
        true
    }

    fn accept(&mut self, ticket: &Ticket) -> bool {
        if self.busy.as_ref() != Some(ticket)
            || ticket.epoch != self.epoch
            || ticket.scope != self.scope
        {
            return false;
        }
        self.busy = None;
        true
    }

    fn disconnect(&mut self) {
        let had_pending_send = self.pending_send.is_some();
        let epoch = self.epoch.wrapping_add(1);
        *self = Self::default();
        self.epoch = epoch;
        self.connection_notice =
            "Disconnected locally. Cloud history and other devices are unchanged.".into();
        if had_pending_send {
            self.send_notice = "A dispatched message may still execute remotely. It was not cancelled or retried. Reconnect and check its original conversation.".into();
        }
    }

    fn account_changed(&mut self) {
        self.disconnect();
        self.connection_notice = "The selected local Muse session changed account identity or agent. All displayed account data was cleared. Review the session and explicitly reconnect.".into();
    }

    fn begin_connect(&mut self, consent: bool) -> Option<Ticket> {
        if !consent {
            self.connection_notice =
                "Explicit consent is required; no account request was made.".into();
            return None;
        }
        if self.busy.is_some() || self.account.is_some() {
            return None;
        }
        self.issue(Operation::Connect)
    }

    fn issue(&mut self, operation: Operation) -> Option<Ticket> {
        if self.busy.is_some() {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        let ticket = Ticket {
            epoch: self.epoch,
            serial: self.serial,
            scope: self.scope.clone(),
            limit: self.limit,
            operation,
        };
        self.busy = Some(ticket.clone());
        Some(ticket)
    }
}

#[cfg(not(account_controller_only))]
pub use native::{AccountPage, build};

#[cfg(not(account_controller_only))]
mod native {
    use super::*;
    use crate::account_login::{Cancellation, run_sign_in};
    use crate::local_host::{AccountContext, BoundaryHook, SharedDevice};
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_account::{
        AccountClient, AccountSnapshot, BridgeConfig, HistoryMessage, TasksSnapshot,
    };
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    #[cfg(test)]
    use std::time::Instant;

    type State = Controller<AccountSnapshot, HistoryMessage, TasksSnapshot>;

    pub struct AccountPage {
        pub root: gtk::Box,
        pub settings: gtk::Expander,
        pub cancel_login: Rc<dyn Fn()>,
        pub host_context: AccountContext,
        pub host_boundary: BoundaryHook,
        pub host_device: SharedDevice,
        #[cfg(test)]
        ui: Rc<Ui>,
    }

    struct Ui {
        state: RefCell<State>,
        client: RefCell<Option<AccountClient>>,
        connected_config: RefCell<Option<BridgeConfig>>,
        account_active: RefCell<Option<Arc<AtomicBool>>>,
        host_verified: Cell<bool>,
        host_boundary: BoundaryHook,
        host_device: SharedDevice,
        login_cancellation: RefCell<Option<Arc<Cancellation>>>,
        rendering: Cell<bool>,
        root: gtk::Box,
        settings: gtk::Expander,
        python: gtk::Entry,
        config_dir: gtk::Entry,
        consent: gtk::CheckButton,
        existing: gtk::Button,
        connect: gtk::Button,
        login: gtk::Button,
        disconnect: gtk::Button,
        connection_notice: gtk::Label,
        identity: gtk::Label,
        chip: gtk::Label,
        threads: gtk::ComboBoxText,
        scope_label: gtk::Label,
        refresh: gtk::Button,
        older: gtk::Button,
        history: gtk::Box,
        history_notice: gtk::Label,
        tasks: gtk::Box,
        tasks_notice: gtk::Label,
        send_notice: gtk::Label,
        acknowledge: gtk::Button,
        prompt: gtk::Entry,
        send: gtk::Button,
        spinner: gtk::Spinner,
    }

    enum Reply {
        Connect(Result<AccountSnapshot, String>),
        Refresh(
            Result<AccountSnapshot, String>,
            Result<Vec<HistoryMessage>, String>,
            Result<TasksSnapshot, String>,
        ),
        History(Result<Vec<HistoryMessage>, String>),
        Send(SendOutcome),
        SendConnectionError(SendOutcome),
        Login(Result<(), String>),
        AccountChanged,
    }

    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }

    fn home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    fn config_home() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home().join(".config"))
    }

    fn default_python() -> PathBuf {
        [
            home().join(".local/share/muse-mcp/.venv/bin/python"),
            home().join(".local/share/unofficial-muse-for-linux/account-bridge/.venv/bin/python"),
        ]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/python3"))
    }

    fn script_path(name: &str) -> PathBuf {
        let mut roots = Vec::new();
        if let Some(appdir) = std::env::var_os("APPDIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            roots.push(appdir);
        }
        if let Ok(executable) = std::env::current_exe() {
            roots.extend(executable.ancestors().skip(1).map(Path::to_path_buf));
        }
        roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
        for root in &roots {
            for path in [
                root.join("usr/share/unofficial-muse-for-linux/account")
                    .join(name),
                root.join("scripts").join(name),
            ] {
                if path.is_file() {
                    return path;
                }
            }
        }
        PathBuf::from("/usr/share/unofficial-muse-for-linux/account").join(name)
    }

    pub fn build(chip: &gtk::Label) -> AccountPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label("Your Muse account · shared conversations & tasks"));
        let settings = gtk::Expander::new(Some("Account connection & sign-in"));
        settings.add_css_class("settings-disclosure");
        settings.set_expanded(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("settings-content");
        content.append(&label("Unsupported community account integration. This links your existing personal Muse agent, not a developer API key and not official Mac-device registration. No embedded website; no password, token or cookie form."));
        content.append(&label("Python executable (absolute path; install the community muse-mcp package in this interpreter's virtualenv):"));
        let python = gtk::Entry::new();
        python.set_text(&default_python().display().to_string());
        content.append(&python);
        content.append(&label(
            "Muse session directory (paths stay in memory; disconnect before changing):",
        ));
        let config_dir = gtk::Entry::new();
        config_dir.set_text(
            &config_home()
                .join("unofficial-muse-for-linux/account")
                .display()
                .to_string(),
        );
        content.append(&config_dir);
        let consent = gtk::CheckButton::with_label(
            "I allow access to this selected Muse session, its account identity, conversations and task history.",
        );
        content.append(&consent);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("toolbar");
        let existing = gtk::Button::with_label("Use existing local Muse session");
        let login = gtk::Button::with_label("Sign in with browser…");
        login.set_tooltip_text(Some("Opens official muse.ai in an isolated Chromium-compatible browser. Not all installed browsers are supported. Writes only the selected app/session directory."));
        let connect = gtk::Button::with_label("Connect");
        connect.add_css_class("suggested-action");
        let disconnect = gtk::Button::with_label("Disconnect");
        for button in [&existing, &login, &connect, &disconnect] {
            actions.append(button);
        }
        content.append(&actions);
        content.append(&label("Browser sign-in uses an isolated Chromium-compatible browser; other browsers may not work. Disconnect only clears this app's memory: it does not revoke other devices, erase saved session credentials or delete cloud history."));
        settings.set_child(Some(&content));
        root.append(&settings);
        let identity = label("Not linked");
        identity.add_css_class("assistant-name");
        root.append(&identity);
        let connection_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        connection_row.append(&spinner);
        let connection_notice = label("");
        connection_notice.set_hexpand(true);
        connection_row.append(&connection_notice);
        root.append(&connection_row);

        let pages = gtk::Notebook::new();
        pages.set_vexpand(true);
        let chat = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.add_css_class("toolbar");
        let threads = gtk::ComboBoxText::new();
        threads.set_hexpand(true);
        threads.set_tooltip_text(Some("Main conversation or actual service-provided side chats. Archived sessions are labeled; no local/synthetic sessions are created."));
        let refresh = gtk::Button::with_label("Refresh shared data");
        let older = gtk::Button::with_label("Load older (+50, max 200)");
        toolbar.append(&threads);
        toolbar.append(&refresh);
        toolbar.append(&older);
        chat.append(&toolbar);
        let scope_label = label("No conversation selected until account connection.");
        chat.append(&scope_label);
        let history = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .min_content_height(160)
            .build();
        scroll.set_child(Some(&history));
        chat.append(&scroll);
        let history_notice = label("");
        history_notice.add_css_class("chat-status");
        chat.append(&history_notice);
        chat.append(&label("Send talks to your existing Muse agent and may trigger external actions. No local files are automatically uploaded. Review the selected conversation and message before pressing Send."));
        let send_notice = label("");
        send_notice.add_css_class("chat-status");
        chat.append(&send_notice);
        let acknowledge = gtk::Button::with_label(
            "I've checked the original shared history — allow another send",
        );
        chat.append(&acknowledge);
        let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        composer.add_css_class("composer");
        let prompt = gtk::Entry::new();
        prompt.set_hexpand(true);
        prompt.set_placeholder_text(Some("Message your existing Muse agent…"));
        let send = gtk::Button::with_label("Send to Muse");
        send.add_css_class("suggested-action");
        composer.append(&prompt);
        composer.append(&send);
        chat.append(&composer);
        pages.append_page(&chat, Some(&gtk::Label::new(Some("Shared chat history"))));

        let task_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        task_page.append(&label("Shared schedules & recent runs · read-only"));
        task_page.append(&label("These are the existing account's service-provided schedules and run records, not Linux-local jobs. This pane cannot create, edit, cancel or retry tasks. Use Refresh shared data or wait for the idle 30-second refresh."));
        let tasks_notice = label("");
        task_page.append(&tasks_notice);
        let tasks = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let tasks_scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        tasks_scroll.set_child(Some(&tasks));
        task_page.append(&tasks_scroll);
        pages.append_page(
            &task_page,
            Some(&gtk::Label::new(Some("Scheduled tasks & runs"))),
        );
        root.append(&pages);

        let ui = Rc::new(Ui {
            state: RefCell::new(State::default()),
            client: RefCell::new(None),
            connected_config: RefCell::new(None),
            account_active: RefCell::new(None),
            host_verified: Cell::new(false),
            host_boundary: Rc::new(RefCell::new(None)),
            host_device: Rc::new(RefCell::new(None)),
            login_cancellation: RefCell::new(None),
            rendering: Cell::new(false),
            root: root.clone(),
            settings: settings.clone(),
            python,
            config_dir,
            consent,
            existing,
            connect,
            login,
            disconnect,
            connection_notice,
            identity,
            chip: chip.clone(),
            threads,
            scope_label,
            refresh,
            older,
            history,
            history_notice,
            tasks,
            tasks_notice,
            send_notice,
            acknowledge,
            prompt,
            send,
            spinner,
        });
        {
            let u = Rc::clone(&ui);
            ui.existing.connect_clicked(move |_| {
                u.config_dir.set_text(&config_home().join("muse-cli").display().to_string());
                u.consent.set_active(false);
                u.state.borrow_mut().connection_notice = "Existing local Muse session selected. Review this path, consent, then click Connect. No account file has been read by this action.".into();
                u.render();
            });
        }
        {
            let u = Rc::clone(&ui);
            ui.connect.connect_clicked(move |_| u.connect_account());
        }
        {
            let u = Rc::clone(&ui);
            ui.login.connect_clicked(move |_| u.sign_in());
        }
        {
            let u = Rc::clone(&ui);
            ui.disconnect.connect_clicked(move |_| {
                u.disconnect_local();
                u.consent.set_active(false);
                u.prompt.set_text("");
                u.render();
            });
        }
        {
            let u = Rc::clone(&ui);
            ui.refresh.connect_clicked(move |_| u.read(false, true));
        }
        {
            let u = Rc::clone(&ui);
            ui.older.connect_clicked(move |_| u.read(true, false));
        }
        {
            let u = Rc::clone(&ui);
            ui.threads.connect_changed(move |combo| {
                if u.rendering.get() {
                    return;
                }
                let Some(id) = combo.active_id() else {
                    return;
                };
                let scope = id.strip_prefix("thread:").map(str::to_string);
                let changed = u.state.borrow_mut().select_thread(scope);
                if changed {
                    u.prompt.set_text("");
                    u.render();
                    u.read(false, false);
                }
            });
        }
        {
            let u = Rc::clone(&ui);
            ui.send.connect_clicked(move |_| u.send_message());
        }
        {
            let u = Rc::clone(&ui);
            ui.acknowledge.connect_clicked(move |_| {
                u.state.borrow_mut().acknowledge_send();
                // Never leave the uncertain text primed for an accidental resend.
                u.prompt.set_text("");
                u.render();
            });
        }
        for entry in [&ui.python, &ui.config_dir] {
            let u = Rc::clone(&ui);
            entry.connect_changed(move |_| {
                u.consent.set_active(false);
            });
        }
        let weak = Rc::downgrade(&ui);
        gtk::glib::timeout_add_local(Duration::from_secs(30), move || {
            let Some(ui) = weak.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            // No session inspection, network request or task polling before explicit connection.
            if ui.root.root().is_none() {
                return gtk::glib::ControlFlow::Break;
            }
            ui.read(false, true);
            gtk::glib::ControlFlow::Continue
        });
        ui.render();
        let weak = Rc::downgrade(&ui);
        let cancel_login = Rc::new(move || {
            if let Some(ui) = weak.upgrade() {
                ui.disconnect_local();
            }
        });
        let weak = Rc::downgrade(&ui);
        let host_context = Rc::new(move || {
            let ui = weak.upgrade()?;
            if ui.state.borrow().account.is_some() && ui.host_verified.get() {
                ui.connected_config.borrow().clone()
            } else {
                None
            }
        });
        AccountPage {
            root,
            settings,
            cancel_login,
            host_context,
            host_boundary: ui.host_boundary.clone(),
            host_device: ui.host_device.clone(),
            #[cfg(test)]
            ui,
        }
    }

    impl Ui {
        fn stop_host(&self) {
            // The registered host owns transport-stop-before-broker-revoke ordering.
            let hook = self.host_boundary.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
            if let Some(device) = self.host_device.borrow_mut().take() {
                device.active.store(false, Ordering::SeqCst);
            }
        }
        fn disconnect_local(&self) {
            self.stop_host();
            self.host_verified.set(false);
            self.cancel_sign_in();
            if let Some(active) = self.account_active.borrow_mut().take() {
                active.store(false, Ordering::SeqCst);
            }
            self.state.borrow_mut().disconnect();
            self.client.borrow_mut().take();
            self.connected_config.borrow_mut().take();
        }

        fn cancel_sign_in(&self) {
            if let Some(cancellation) = self.login_cancellation.borrow_mut().take() {
                cancellation.cancel();
            }
        }

        fn config(&self) -> Result<BridgeConfig, String> {
            let python = PathBuf::from(self.python.text().as_str());
            let config_dir = PathBuf::from(self.config_dir.text().as_str());
            if !python.is_absolute() || !python.is_file() {
                return Err(
                    "Choose an existing absolute Python executable path. No request was made."
                        .into(),
                );
            }
            if !config_dir.is_absolute() {
                return Err(
                    "Choose an absolute Muse session directory. No request was made.".into(),
                );
            }
            let script = script_path("muse_account_bridge.py");
            if !script.is_file() {
                return Err("The packaged account bridge script is missing. Install it under usr/share/unofficial-muse-for-linux/account or run from a source checkout.".into());
            }
            Ok(BridgeConfig {
                python,
                script,
                config_dir,
            })
        }

        fn connect_account(self: &Rc<Self>) {
            let ticket = self
                .state
                .borrow_mut()
                .begin_connect(self.consent.is_active());
            let Some(ticket) = ticket else {
                self.render();
                return;
            };
            let config = match self.config() {
                Ok(config) => config,
                Err(error) => {
                    self.state.borrow_mut().finish_connect(&ticket, Err(error));
                    self.render();
                    return;
                }
            };
            *self.connected_config.borrow_mut() = Some(config.clone());
            let active = Arc::new(AtomicBool::new(true));
            *self.account_active.borrow_mut() = Some(active.clone());
            let client = AccountClient::new(config).cancellable(active);
            *self.client.borrow_mut() = Some(client.clone());
            self.state.borrow_mut().connection_notice =
                "Connecting to the explicitly selected Muse session…".into();
            self.launch(ticket, move || {
                Reply::Connect(client.connect().map_err(|e| e.to_string()))
            });
        }

        fn sign_in(self: &Rc<Self>) {
            let ticket = self
                .state
                .borrow_mut()
                .begin_login(self.consent.is_active());
            let Some(ticket) = ticket else {
                self.render();
                return;
            };
            let config = match self.config() {
                Ok(config) => config,
                Err(error) => {
                    self.state.borrow_mut().finish_login(&ticket, Err(error));
                    self.render();
                    return;
                }
            };
            let script = script_path("muse_account_login.py");
            let cancellation = Arc::new(Cancellation::default());
            *self.login_cancellation.borrow_mut() = Some(Arc::clone(&cancellation));
            self.launch(ticket, move || {
                Reply::Login(run_sign_in(
                    &config.python,
                    &script,
                    &config.config_dir,
                    &cancellation,
                ))
            });
        }

        fn expected_identity(&self) -> Option<(String, String)> {
            self.state
                .borrow()
                .account
                .as_ref()
                .map(|a| (a.identity_name.clone(), a.vm_id.clone()))
        }

        fn read(self: &Rc<Self>, older: bool, refresh: bool) {
            let Some(client) = self.client.borrow().clone() else {
                return;
            };
            let Some(identity) = self.expected_identity() else {
                return;
            };
            let Some(ticket) = self.state.borrow_mut().begin_read(older, refresh) else {
                return;
            };
            let request = ticket.clone();
            self.launch(ticket, move || {
                let account = client.connect().map_err(|e| e.to_string());
                match &account {
                    Ok(a)
                        if (a.identity_name.as_str(), a.vm_id.as_str())
                            != (identity.0.as_str(), identity.1.as_str()) =>
                    {
                        return Reply::AccountChanged;
                    }
                    Err(error) => {
                        return if refresh {
                            Reply::Refresh(
                                account.clone(),
                                Err(format!("account verification failed: {error}")),
                                Err(format!("account verification failed: {error}")),
                            )
                        } else {
                            Reply::History(Err(format!("account verification failed: {error}")))
                        };
                    }
                    _ => {}
                }
                if refresh {
                    let (history, tasks) = std::thread::scope(|scope| {
                        let history = scope.spawn(|| {
                            client
                                .history(request.scope.as_deref(), request.limit)
                                .map_err(|e| e.to_string())
                        });
                        let tasks = scope.spawn(|| client.tasks().map_err(|e| e.to_string()));
                        (
                            history.join().unwrap_or_else(|_| {
                                Err("History worker stopped unexpectedly.".into())
                            }),
                            tasks.join().unwrap_or_else(|_| {
                                Err("Task-history worker stopped unexpectedly.".into())
                            }),
                        )
                    });
                    Reply::Refresh(account, history, tasks)
                } else {
                    Reply::History(
                        client
                            .history(request.scope.as_deref(), request.limit)
                            .map_err(|e| e.to_string()),
                    )
                }
            });
        }

        fn send_message(self: &Rc<Self>) {
            let Some(client) = self.client.borrow().clone() else {
                self.state.borrow_mut().send_notice =
                    "Not connected. Connect your Muse session before sending.".into();
                self.render();
                return;
            };
            let Some(identity) = self.expected_identity() else {
                self.state.borrow_mut().send_notice =
                    "Not connected. Connect your Muse session before sending.".into();
                self.render();
                return;
            };
            let text = self.prompt.text().to_string();
            let Some(ticket) = self.state.borrow_mut().begin_send(&text) else {
                let reason = self.state.borrow().describe_send_block(&text);
                self.state.borrow_mut().send_notice = reason.into();
                self.render();
                return;
            };
            let request = ticket.clone();
            let device = self
                .host_device
                .borrow()
                .clone()
                .filter(|d| d.active.load(Ordering::SeqCst));
            let client = if let Some(device) = &device {
                client.cancellable(device.active.clone())
            } else {
                client
            };
            self.launch(ticket, move || {
                match client.connect() {
                    Ok(a)
                        if (a.identity_name.as_str(), a.vm_id.as_str())
                            != (identity.0.as_str(), identity.1.as_str()) =>
                    {
                        return Reply::AccountChanged;
                    }
                    Err(e) => return Reply::Send(SendOutcome::NotSent(e.to_string())),
                    _ => {}
                }
                let dispatch = match device {
                    Some(device) if device.active.load(Ordering::SeqCst) => {
                        client.send_from_device(request.scope.as_deref(), &text, &device.node_id)
                    }
                    Some(_) => {
                        return Reply::Send(SendOutcome::NotSent(
                            "Local host stopped before dispatch.".into(),
                        ));
                    }
                    None => client.send(request.scope.as_deref(), &text),
                };
                let result = match dispatch {
                    Ok(receipt) if !receipt.sent => {
                        SendOutcome::NotSent("Service did not accept the dispatch.".into())
                    }
                    Ok(receipt) if receipt.confirmed => SendOutcome::Confirmed,
                    Ok(_) => SendOutcome::Uncertain(
                        "Service dispatch returned without scoped-history confirmation.".into(),
                    ),
                    Err(error) => {
                        return Reply::SendConnectionError(if error.uncertain {
                            SendOutcome::Uncertain(error.to_string())
                        } else {
                            SendOutcome::NotSent(error.to_string())
                        });
                    }
                };
                Reply::Send(result)
            });
        }

        fn launch(self: &Rc<Self>, ticket: Ticket, work: impl FnOnce() -> Reply + Send + 'static) {
            self.render();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let reply = work();
                let _ = tx.send(reply);
            });
            let weak = Rc::downgrade(self);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                if ui.state.borrow().busy.as_ref() != Some(&ticket) {
                    // A disconnected account or different thread must never receive this result.
                    return gtk::glib::ControlFlow::Break;
                }
                let reply = match rx.try_recv() {
                    Ok(reply) => reply,
                    Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => match ticket.operation {
                        Operation::Connect => {
                            Reply::Connect(Err("Account worker stopped unexpectedly.".into()))
                        }
                        Operation::Login => {
                            Reply::Login(Err("Sign-in worker stopped unexpectedly.".into()))
                        }
                        Operation::Send => Reply::SendConnectionError(SendOutcome::Uncertain(
                            "Dispatch worker stopped unexpectedly.".into(),
                        )),
                        Operation::Refresh => Reply::Refresh(
                            Err("Account worker stopped unexpectedly.".into()),
                            Err("History worker stopped unexpectedly.".into()),
                            Err("Task-history worker stopped unexpectedly.".into()),
                        ),
                        Operation::History => {
                            Reply::History(Err("History worker stopped unexpectedly.".into()))
                        }
                    },
                };
                ui.complete(&ticket, reply);
                gtk::glib::ControlFlow::Break
            });
        }

        fn complete(self: &Rc<Self>, ticket: &Ticket, reply: Reply) {
            if self.state.borrow().busy.as_ref() != Some(ticket) {
                return;
            }
            let mut refresh_after = false;
            let send_connection_error = matches!(&reply, Reply::SendConnectionError(_));
            // Stop immediately on account verification errors or identity changes,
            // before any old account result is installed or rendered.
            if matches!(
                &reply,
                Reply::AccountChanged
                    | Reply::Refresh(Err(_), _, _)
                    | Reply::Refresh(_, Err(_), _)
                    | Reply::Refresh(_, _, Err(_))
                    | Reply::History(Err(_))
                    | Reply::Send(SendOutcome::NotSent(_))
                    | Reply::SendConnectionError(_)
            ) {
                self.host_verified.set(false);
                self.stop_host();
            }
            if matches!(
                &reply,
                Reply::Connect(Ok(_)) | Reply::Refresh(Ok(_), Ok(_), Ok(_))
            ) {
                self.host_verified.set(true);
            }
            {
                let mut state = self.state.borrow_mut();
                match reply {
                    Reply::Connect(result) => {
                        refresh_after = result.is_ok();
                        state.finish_connect(ticket, result);
                        if !refresh_after {
                            self.client.borrow_mut().take();
                        }
                    }
                    Reply::Refresh(account, history, tasks) => {
                        state.finish_refresh(ticket, account, history, tasks);
                    }
                    Reply::History(result) => {
                        state.finish_history(ticket, result);
                    }
                    Reply::Send(result) | Reply::SendConnectionError(result) => {
                        let confirmed = matches!(&result, SendOutcome::Confirmed);
                        state.finish_send(ticket, result);
                        if confirmed {
                            self.prompt.set_text("");
                        }
                        refresh_after = !send_connection_error;
                    }
                    Reply::Login(result) => {
                        self.login_cancellation.borrow_mut().take();
                        state.finish_login(ticket, result);
                    }
                    Reply::AccountChanged => {
                        state.account_changed();
                        if let Some(active) = self.account_active.borrow_mut().take() {
                            active.store(false, Ordering::SeqCst);
                        }
                        self.connected_config.borrow_mut().take();
                        self.client.borrow_mut().take();
                        self.consent.set_active(false);
                        self.prompt.set_text("");
                    }
                }
            }
            self.render();
            if refresh_after {
                self.read(false, true);
            }
        }

        fn render(&self) {
            self.rendering.set(true);
            let state = self.state.borrow();
            let connected = state.account.is_some();
            let idle = state.busy.is_none();
            let settings_enabled = !connected && idle;
            for widget in [
                self.python.upcast_ref::<gtk::Widget>(),
                self.config_dir.upcast_ref(),
                self.consent.upcast_ref(),
                self.existing.upcast_ref(),
                self.connect.upcast_ref(),
                self.login.upcast_ref(),
            ] {
                widget.set_sensitive(settings_enabled);
            }
            self.disconnect.set_sensitive(connected || !idle);
            self.refresh.set_sensitive(connected && idle);
            self.older
                .set_sensitive(connected && idle && state.limit < 200);
            self.threads.set_sensitive(connected && idle);
            self.prompt
                .set_sensitive(connected && idle && state.pending_send.is_none());
            self.send
                .set_sensitive(connected && idle && state.pending_send.is_none());
            self.acknowledge
                .set_visible(state.pending_send.is_some() && idle);
            self.acknowledge.set_sensitive(connected && idle);
            self.spinner.set_spinning(!idle);
            let busy = state
                .busy
                .as_ref()
                .map(|t| match t.operation {
                    Operation::Connect => " · connecting…",
                    Operation::Refresh => " · refreshing account, history and tasks…",
                    Operation::History => " · loading scoped history…",
                    Operation::Send => " · dispatching once…",
                    Operation::Login => " · browser sign-in in progress…",
                })
                .unwrap_or("");
            self.connection_notice
                .set_text(&format!("{}{busy}", state.connection_notice));
            self.identity.set_text(
                &state
                    .account
                    .as_ref()
                    .map(|a| format!("{} · existing Muse agent {}", a.identity_name, a.vm_id))
                    .unwrap_or_else(|| "Not linked".into()),
            );
            self.chip.set_text(if connected {
                "Muse account linked • no automatic file sharing"
            } else {
                "Muse account disconnected • local files read-only"
            });
            self.threads.remove_all();
            self.threads.append(Some("main"), "Main conversation");
            if let Some(account) = &state.account {
                for thread in &account.threads {
                    let archived = if thread.archived { " [archived]" } else { "" };
                    let updated = thread
                        .updated_at
                        .as_deref()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default();
                    self.threads.append(
                        Some(&format!("thread:{}", thread.session_id)),
                        &format!("{}{archived}{updated}", thread.title),
                    );
                }
            }
            let selected = state
                .scope
                .as_ref()
                .map(|id| format!("thread:{id}"))
                .unwrap_or_else(|| "main".into());
            // A disappearing service thread keeps its exact scope; never silently fall back to main.
            if !self.threads.set_active_id(Some(&selected)) {
                self.threads.append(
                    Some(&selected),
                    "Selected conversation (not in current service thread list)",
                );
                self.threads.set_active_id(Some(&selected));
            }
            self.scope_label.set_text(&format!(
                "Conversation scope: {} · latest {} service events",
                state.scope.as_deref().unwrap_or("Main conversation"),
                state.limit
            ));
            self.history_notice.set_text(&state.history_notice);
            self.tasks_notice.set_text(&state.tasks_notice);
            self.send_notice.set_text(&state.send_notice);
            super::super::clear_chat(&self.history);
            if !state.history.is_empty() {
                super::super::append_day_divider(&self.history);
            }
            for message in &state.history {
                let role = match message.role.as_str() {
                    "user" => super::super::ChatRole::User,
                    "assistant" => super::super::ChatRole::Assistant,
                    _ => super::super::ChatRole::Notice,
                };
                let metadata = format!(
                    "{} · seq {}{}{}",
                    message.role,
                    message.seq,
                    message
                        .timestamp
                        .as_deref()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default(),
                    message
                        .status
                        .as_deref()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default()
                );
                let details = label(&metadata);
                details.add_css_class("dim-label");
                details.add_css_class("message-meta");
                self.history.append(&details);
                super::super::append_chat(&self.history, role, &message.text);
            }
            super::super::clear_chat(&self.tasks);
            if let Some(tasks) = &state.tasks {
                self.tasks.append(&label(&format!(
                    "{} schedules · {} recent runs returned",
                    tasks.schedules.len(),
                    tasks.runs.len()
                )));
                for schedule in &tasks.schedules {
                    let item = label(&format!(
                        "{}\nID: {} · {} · timezone: {} · next run: {}",
                        schedule.title,
                        schedule.id,
                        if schedule.enabled {
                            "Enabled"
                        } else {
                            "Disabled"
                        },
                        schedule.timezone.as_deref().unwrap_or("not supplied"),
                        utc_time(schedule.next_run_at_utc)
                    ));
                    item.add_css_class("card");
                    self.tasks.append(&item);
                }
                self.tasks
                    .append(&label("Recent task runs (no retry controls)"));
                for run in &tasks.runs {
                    let item = label(&format!(
                        "{} · job {} · {}\nScheduled: {}\n{}{}",
                        run.run_id,
                        run.job_id,
                        run.status,
                        utc_time(run.scheduled_for_utc),
                        run.result_summary
                            .as_deref()
                            .unwrap_or("No result summary supplied."),
                        run.error_text
                            .as_deref()
                            .map(|e| format!("\nRun error: {e}"))
                            .unwrap_or_default()
                    ));
                    item.add_css_class("card");
                    self.tasks.append(&item);
                }
            }
            if connected
                && self.settings.is_expanded()
                && state.history_notice.starts_with("No shared")
            {
                self.settings.set_expanded(false);
            }
            self.rendering.set(false);
        }
    }

    fn utc_time(timestamp: Option<i64>) -> String {
        timestamp
            .map(|value| {
                gtk::glib::DateTime::from_unix_utc(value)
                    .and_then(|date| date.format("%Y-%m-%d %H:%M:%S UTC"))
                    .map(|date| date.to_string())
                    .unwrap_or_else(|_| format!("Unix {value} (UTC)"))
            })
            .unwrap_or_else(|| "not supplied".into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
            let mut result = vec![root.clone()];
            let mut child = root.first_child();
            while let Some(widget) = child {
                result.extend(descendants(&widget));
                child = widget.next_sibling();
            }
            result
        }

        fn button(root: &gtk::Box, text: &str) -> gtk::Button {
            descendants(root.upcast_ref())
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
                .find(|button| button.label().as_deref() == Some(text))
                .unwrap()
        }

        fn has_label(root: &gtk::Box, text: &str) -> bool {
            descendants(root.upcast_ref())
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
                .any(|label| label.text().contains(text))
        }

        fn pump_until(mut predicate: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            let context = gtk::glib::MainContext::default();
            while !predicate() {
                assert!(
                    Instant::now() < deadline,
                    "GTK condition did not complete within 20 seconds"
                );
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
        fn native_account_context_device_dispatch_and_disconnect_coupling() {
            use crate::local_host::DeviceLease;
            use std::sync::atomic::{AtomicBool, Ordering};
            gtk::init().unwrap();
            let scratch = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap()
                .join(format!(
                    "account-host-ui-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
            std::fs::create_dir(&scratch).unwrap();
            let script = scratch.join("bridge.py");
            std::fs::write(
                &script,
                r#"
import json,sys,pathlib
r=json.load(sys.stdin)
if r['op']=='send':
 pathlib.Path(__file__).with_name('sent').write_text(json.dumps(r))
 data={'sent':True,'confirmed':False,'message_id':None}
elif r['op']=='connect': data={'identity_name':'fake','vm_id':'fake-vm','threads':[]}
elif r['op']=='tasks': data={'schedules':[],'runs':[]}
else: data=[]
print(json.dumps({'ok':True,'data':data}))
"#,
            )
            .unwrap();
            let config = BridgeConfig {
                python: "/usr/bin/python3".into(),
                script,
                config_dir: scratch.clone(),
            };
            let page = build(&gtk::Label::new(None));
            assert!((page.host_context)().is_none());
            page.ui.host_verified.set(true);
            *page.ui.connected_config.borrow_mut() = Some(config.clone());
            *page.ui.client.borrow_mut() = Some(AccountClient::new(config.clone()));
            page.ui.state.borrow_mut().account = Some(AccountSnapshot {
                identity_name: "fake".into(),
                vm_id: "fake-vm".into(),
                threads: vec![],
            });
            assert_eq!((page.host_context)().unwrap().script, config.script);
            let boundary = Rc::new(Cell::new(0));
            let count = boundary.clone();
            *page.host_boundary.borrow_mut() = Some(Rc::new(move || count.set(count.get() + 1)));
            let active = Arc::new(AtomicBool::new(true));
            *page.host_device.borrow_mut() = Some(DeviceLease {
                node_id: "linux-registered-test".into(),
                active: active.clone(),
            });
            page.ui.prompt.set_text("once");
            page.ui.send_message();
            pump_until(|| page.ui.state.borrow().busy.is_none());
            let sent: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(scratch.join("sent")).unwrap())
                    .unwrap();
            assert_eq!(sent["device_id"], "linux-registered-test");
            page.ui.state.borrow_mut().acknowledge_send();
            page.host_device.borrow_mut().take();
            active.store(false, Ordering::SeqCst);
            page.ui.prompt.set_text("after stop");
            page.ui.send_message();
            pump_until(|| page.ui.state.borrow().busy.is_none());
            let sent: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(scratch.join("sent")).unwrap())
                    .unwrap();
            assert!(sent.get("device_id").is_none());
            let refresh = page.ui.state.borrow_mut().begin_read(false, true).unwrap();
            page.ui.complete(
                &refresh,
                Reply::Refresh(
                    Ok(AccountSnapshot {
                        identity_name: "fake".into(),
                        vm_id: "fake-vm".into(),
                        threads: vec![],
                    }),
                    Err("connection failed".into()),
                    Ok(TasksSnapshot {
                        schedules: vec![],
                        runs: vec![],
                    }),
                ),
            );
            assert_eq!(
                boundary.get(),
                1,
                "a failed account IO boundary must stop the host"
            );
            assert!((page.host_context)().is_none());
            let stale = refresh.clone();
            page.ui.complete(
                &stale,
                Reply::Refresh(
                    Ok(AccountSnapshot {
                        identity_name: "fake".into(),
                        vm_id: "fake-vm".into(),
                        threads: vec![],
                    }),
                    Ok(vec![]),
                    Ok(TasksSnapshot {
                        schedules: vec![],
                        runs: vec![],
                    }),
                ),
            );
            assert!(
                (page.host_context)().is_none(),
                "stale successful refresh cannot re-enable host context"
            );
            page.ui.state.borrow_mut().acknowledge_send();
            let failed_send = page
                .ui
                .state
                .borrow_mut()
                .begin_send("connection error")
                .unwrap();
            page.ui.complete(
                &failed_send,
                Reply::SendConnectionError(SendOutcome::Uncertain(
                    "connection closed after dispatch".into(),
                )),
            );
            assert_eq!(boundary.get(), 2);
            page.ui.disconnect.emit_clicked();
            assert_eq!(boundary.get(), 3);
            assert!((page.host_context)().is_none());
            assert!(page.host_device.borrow().is_none());
            std::fs::remove_dir_all(scratch).unwrap();
        }

        #[test]
        fn schedule_times_are_readable_utc_without_inventing_missing_values() {
            assert_eq!(utc_time(Some(0)), "1970-01-01 00:00:00 UTC");
            assert_eq!(utc_time(None), "not supplied");
            assert!(utc_time(Some(i64::MAX)).contains("UTC"));
        }

        #[test]
        #[ignore = "requires GTK display; run this filter under xvfb-run, --test-threads=1"]
        fn native_account_page_consent_failure_and_disconnect_signal_smoke() {
            gtk::init().unwrap();
            let chip = gtk::Label::new(None);
            let page = build(&chip);
            let window = gtk::Window::builder()
                .default_width(1240)
                .default_height(900)
                .build();
            window.set_child(Some(&page.root));
            window.present();
            let controls = descendants(page.root.upcast_ref());
            let entries: Vec<_> = controls
                .iter()
                .cloned()
                .filter_map(|widget| widget.downcast::<gtk::Entry>().ok())
                .collect();
            let consent = controls
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::CheckButton>().ok())
                .unwrap();
            assert_eq!(entries.len(), 3);
            assert!(!entries[2].is_sensitive());
            button(&page.root, "Connect").emit_clicked();
            assert!(has_label(&page.root, "Explicit consent is required"));
            button(&page.root, "Use existing local Muse session").emit_clicked();
            assert_eq!(
                entries[1].text().as_str(),
                config_home().join("muse-cli").to_str().unwrap()
            );
            assert!(!consent.is_active());
            entries[0].set_text("/usr/bin/python3");
            let scratch = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .expect("Set TMPDIR to the test scratch directory");
            let missing_config =
                scratch.join(format!("muse-ui-missing-config-{}", std::process::id()));
            assert!(!missing_config.exists());
            entries[1].set_text(missing_config.to_str().unwrap());
            consent.set_active(true);
            button(&page.root, "Connect").emit_clicked();
            assert!(!entries[0].is_sensitive());
            pump_until(|| has_label(&page.root, "Connection failed:"));
            assert!(chip.text().contains("disconnected"));
            assert!(entries[0].is_sensitive());
            assert!(!entries[2].is_sensitive());
            button(&page.root, "Connect").emit_clicked();
            button(&page.root, "Disconnect").emit_clicked();
            assert!(has_label(&page.root, "Disconnected locally"));
            assert!(!consent.is_active());
            // Run a local main-context timer to prove the GUI remains responsive after invalidation.
            let ticked = Rc::new(Cell::new(false));
            let probe = Rc::clone(&ticked);
            gtk::glib::timeout_add_local_once(Duration::from_millis(200), move || probe.set(true));
            pump_until(|| ticked.get());
            assert!(has_label(&page.root, "Disconnected locally"));
            assert!(has_label(&page.root, "No shared history requested"));
            window.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type State = Controller<String, String, String>;

    #[test]
    fn helper_error_codes_are_actionable_without_echoing_untrusted_output() {
        assert!(login_error_message("missing_browser").contains("Chromium"));
        assert!(login_error_message("missing_dependency").contains("Python"));
        assert!(login_error_message("unsafe_config").contains("private"));
        assert!(login_error_message("unsafe_devtools").contains("verified"));
        assert!(!login_error_message("cookie=secret-never-show").contains("secret-never-show"));
    }

    #[test]
    fn sign_in_requires_consent_and_does_not_auto_connect_or_poll() {
        let mut state = State::default();
        assert!(state.begin_login(false).is_none());
        let ticket = state.begin_login(true).unwrap();
        assert!(state.begin_connect(true).is_none());
        state.finish_login(&ticket, Ok(()));
        assert!(state.account.is_none());
        assert!(state.begin_read(false, true).is_none());
        assert!(state.connection_notice.contains("Connect"));
    }

    #[test]
    fn externally_changed_account_requires_fresh_consent_without_old_data() {
        let mut state = connected();
        state.history.push("private".into());
        state.tasks = Some("private tasks".into());
        let old = state.begin_read(false, true).unwrap();
        state.account_changed();
        assert!(state.account.is_none());
        assert!(state.history.is_empty());
        assert!(state.tasks.is_none());
        assert!(state.connection_notice.contains("changed"));
        assert!(!state.finish_refresh(
            &old,
            Ok("new identity".into()),
            Ok(vec!["new data".into()]),
            Ok("new tasks".into())
        ));
        assert!(state.begin_read(false, true).is_none());
    }

    fn connected() -> State {
        let mut state = State::default();
        let ticket = state.begin_connect(true).unwrap();
        state.finish_connect(&ticket, Ok("account".into()));
        state
    }

    #[test]
    fn disconnected_controller_never_polls_or_dispatches() {
        let mut state = State::default();
        assert!(state.begin_read(false, true).is_none());
        assert!(state.begin_read(true, false).is_none());
        assert!(state.begin_send("must not leave this device").is_none());
        assert!(state.busy.is_none());
    }

    #[test]
    fn returning_to_same_scope_still_rejects_old_epoch() {
        let mut state = connected();
        let old_main = state.begin_read(false, false).unwrap();
        state.select_thread(Some("other".into()));
        state.select_thread(None);
        let new_main = state.begin_read(false, false).unwrap();
        assert!(!state.finish_history(&old_main, Ok(vec!["stale same-scope message".into()])));
        assert_eq!(state.busy.as_ref(), Some(&new_main));
        state.finish_history(&new_main, Ok(vec!["fresh".into()]));
        assert_eq!(state.history, vec!["fresh"]);
    }

    #[test]
    fn disconnect_during_dispatch_clears_private_text_and_rejects_completion() {
        let mut state = connected();
        let old = state.begin_send("private message").unwrap();
        state.disconnect();
        assert!(state.pending_send.is_none());
        assert!(state.send_notice.contains("not cancelled or retried"));
        assert!(!state.finish_send(&old, SendOutcome::Confirmed));
        assert!(state.account.is_none());
        assert!(state.history.is_empty());
    }

    #[test]
    fn confirmed_and_known_unsent_results_do_not_trigger_another_dispatch() {
        let mut state = connected();
        let first = state.begin_send("once").unwrap();
        state.finish_send(&first, SendOutcome::Confirmed);
        assert!(state.pending_send.is_none());
        assert!(state.busy.is_none());
        assert!(state.send_notice.contains("does not mean"));
        let second = state.begin_send("explicit second attempt").unwrap();
        state.finish_send(
            &second,
            SendOutcome::NotSent("authentication failed before dispatch".into()),
        );
        assert!(state.pending_send.is_none());
        assert!(state.busy.is_none());
        assert!(state.send_notice.contains("Not sent:"));
        assert!(!state.finish_send(&first, SendOutcome::Confirmed));
    }

    #[test]
    fn refresh_surfaces_each_failure_and_retains_last_successful_data() {
        let mut state = connected();
        state.history.push("previous history".into());
        state.tasks = Some("previous tasks".into());
        let ticket = state.begin_read(false, true).unwrap();
        state.finish_refresh(
            &ticket,
            Err("authentication required".into()),
            Err("network timeout".into()),
            Err("task service unavailable".into()),
        );
        assert!(state.connection_notice.contains("authentication required"));
        assert!(state.history_notice.contains("network timeout"));
        assert!(state.tasks_notice.contains("task service unavailable"));
        assert_eq!(state.history, vec!["previous history"]);
        assert_eq!(state.tasks.as_deref(), Some("previous tasks"));
        assert!(state.busy.is_none());
    }

    #[test]
    fn uncertain_send_retains_scope_blocks_resend_and_survives_history_refresh() {
        let mut state = connected();
        state.select_thread(Some("original-thread".into()));
        let ticket = state.begin_send("original unmodified text ").unwrap();
        assert!(state.begin_send("duplicate").is_none());
        assert!(!state.select_thread(Some("different-thread".into())));
        state.finish_send(&ticket, SendOutcome::Uncertain("deadline exceeded".into()));
        assert_eq!(
            state.pending_send.as_ref().unwrap(),
            &PendingSend {
                scope: Some("original-thread".into()),
                text: "original unmodified text ".into()
            }
        );
        assert!(state.send_notice.contains("deadline exceeded"));
        assert!(state.begin_send("duplicate").is_none());
        let refresh = state.begin_read(false, true).unwrap();
        state.finish_refresh(
            &refresh,
            Ok("account".into()),
            Ok(vec![]),
            Ok("tasks".into()),
        );
        assert!(state.pending_send.is_some());
        assert!(state.send_notice.contains("deadline exceeded"));
        state.acknowledge_send();
        assert!(state.begin_send("a deliberate new message").is_some());
    }

    #[test]
    fn blocked_send_explains_why_instead_of_silently_doing_nothing() {
        // Regression for the user-visible "chat doesn't work" report: every
        // begin_send refusal must map to an actionable notice, never silence.
        let mut disconnected = State::default();
        assert!(disconnected.begin_send("hello").is_none());
        assert!(
            disconnected
                .describe_send_block("hello")
                .contains("Connect")
        );
        let mut ready = connected();
        assert!(ready.begin_send("   ").is_none());
        assert!(ready.describe_send_block("   ").contains("Enter"));
        let _busy = ready.begin_send("first").unwrap();
        assert!(ready.describe_send_block("second").contains("already"));
        ready.finish_send(&_busy, SendOutcome::Uncertain("deadline exceeded".into()));
        assert!(
            ready
                .describe_send_block("second")
                .contains("allow another send")
        );
    }

    #[test]
    fn history_window_expands_only_explicitly_is_capped_and_resets_per_thread() {
        let mut state = connected();
        for expected in [100, 150, 200] {
            let ticket = state.begin_read(true, false).unwrap();
            assert_eq!(ticket.limit, expected);
            assert!(state.begin_read(true, false).is_none());
            assert_eq!(state.limit, expected);
            state.finish_history(&ticket, Ok(vec![]));
            let refresh = state.begin_read(false, true).unwrap();
            assert_eq!(refresh.limit, expected);
            state.finish_history(&refresh, Ok(vec![]));
        }
        assert!(state.begin_read(true, false).is_none());
        state.select_thread(Some("second".into()));
        assert_eq!(state.limit, 50);
    }

    #[test]
    fn thread_switch_clears_history_and_rejects_inflight_old_thread_results() {
        let mut state = connected();
        state.history.push("main message".into());
        let main = state.begin_read(false, false).unwrap();
        assert!(state.select_thread(Some("actual-service-session".into())));
        assert!(state.history.is_empty());
        let selected = state.begin_read(false, false).unwrap();
        assert_eq!(selected.scope.as_deref(), Some("actual-service-session"));
        assert!(!state.finish_history(&main, Ok(vec!["stale main".into()])));
        assert_eq!(state.busy.as_ref(), Some(&selected));
        assert!(state.finish_history(&selected, Ok(vec!["thread message".into()])));
        assert_eq!(state.history, vec!["thread message"]);
    }

    #[test]
    fn disconnect_clears_account_data_and_rejects_previous_account_completion() {
        let mut state = State::default();
        let old = state.begin_connect(true).unwrap();
        assert!(state.finish_connect(&old, Ok("first account".into())));
        state.history.push("private old message".into());
        state.tasks = Some("old tasks".into());
        state.disconnect();
        assert!(state.account.is_none());
        assert!(state.history.is_empty());
        assert!(state.tasks.is_none());
        let fresh = state.begin_connect(true).unwrap();
        assert!(!state.finish_connect(&old, Ok("stale account".into())));
        assert_eq!(state.busy.as_ref(), Some(&fresh));
        assert!(state.finish_connect(&fresh, Ok("second account".into())));
        assert_eq!(state.account.as_deref(), Some("second account"));
    }

    #[test]
    fn connection_locks_other_connect_attempts_until_completion() {
        let mut state = State::default();
        let ticket = state.begin_connect(true).unwrap();
        assert_eq!(state.busy.as_ref(), Some(&ticket));
        assert!(state.begin_connect(true).is_none());
    }

    #[test]
    fn connect_requires_explicit_session_access_consent() {
        let mut state = State::default();
        assert!(state.begin_connect(false).is_none());
        assert!(state.busy.is_none());
        assert!(state.account.is_none());
    }
}
