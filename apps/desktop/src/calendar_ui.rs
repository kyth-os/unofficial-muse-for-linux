//! Native CalDAV page: explicit connection, bounded reads, and one-shot event approval.
//!
//! The controller below is GTK-free so busy/epoch/approval invariants stay
//! unit-testable without a display. The native shell starts every provider
//! operation only from an explicit button and runs it on a worker thread.
#![allow(unexpected_cfgs)]

use muse_calendar::CalError;

const MAX_EVENT_RANGE_SECS: i64 = 31 * 86_400;
const MAX_EVENT_RANGE_SECS_PIN: () = {
    assert!(MAX_EVENT_RANGE_SECS > 0);
    assert!(MAX_EVENT_RANGE_SECS <= 31 * 86_400);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Discover,
    Events,
    Approve,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestTicket {
    epoch: u64,
    operation: Operation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplyDisposition {
    Applied,
    Stale,
}

#[derive(Debug, Default)]
struct StagedEvent {
    id: u64,
    preview: String,
    generation: u64,
}

/// GTK-free decision/state layer for the calendar page.
pub(crate) struct CalendarController {
    epoch: u64,
    active: Option<RequestTicket>,
    staged: Option<StagedEvent>,
}

impl CalendarController {
    pub(crate) fn new() -> Self {
        Self {
            epoch: 0,
            active: None,
            staged: None,
        }
    }

    pub(crate) fn begin(&mut self, operation: Operation) -> Result<RequestTicket, &'static str> {
        if self.active.is_some() {
            return Err("A calendar request is already running. Wait for it to finish first.");
        }
        self.epoch = self.epoch.wrapping_add(1);
        let ticket = RequestTicket {
            epoch: self.epoch,
            operation,
        };
        self.active = Some(ticket);
        Ok(ticket)
    }

    /// Invalidate a worker result when the page abandons the current operation.
    /// The next reply is stale and cannot change UI state.
    pub(crate) fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active = None;
    }

    pub(crate) fn finish(&mut self, ticket: RequestTicket) -> ReplyDisposition {
        if self.active == Some(ticket) {
            self.active = None;
            ReplyDisposition::Applied
        } else {
            ReplyDisposition::Stale
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.active.is_some()
    }

    /// Replace the locally staged event and return the old id for local denial.
    /// The native shell uses that id to drop the old PendingCreate so restaging
    /// never leaves an orphaned approval in the provider client.
    pub(crate) fn stage(&mut self, id: u64, preview: String, generation: u64) -> Option<u64> {
        self.staged
            .replace(StagedEvent {
                id,
                preview,
                generation,
            })
            .map(|old| old.id)
    }

    pub(crate) fn preview(&self) -> Option<&str> {
        self.staged.as_ref().map(|staged| staged.preview.as_str())
    }

    pub(crate) fn take_for_allow(&mut self, generation: u64) -> Option<u64> {
        if self
            .staged
            .as_ref()
            .is_some_and(|staged| staged.generation == generation)
        {
            self.staged.take().map(|staged| staged.id)
        } else {
            None
        }
    }

    pub(crate) fn clear_stage(&mut self) -> Option<u64> {
        self.staged.take().map(|staged| staged.id)
    }

    pub(crate) fn deny_staged(&mut self) -> Option<u64> {
        self.staged.take().map(|staged| staged.id)
    }

    pub(crate) fn notice_for_error(error: &CalError) -> &'static str {
        match error {
            CalError::Auth => "Authentication failed. Check the username/password and reconnect.",
            CalError::NotFound => {
                "The calendar object was not found. Check the selected calendar and base path."
            }
            CalError::Conflict => {
                "Conflict: the event already exists; not overwritten. Choose different event details and stage again."
            }
            CalError::Uncertain => {
                "Create outcome is uncertain: the request may have reached the server. Do not retry automatically; verify the provider first."
            }
            CalError::TamperedDraft => {
                "The staged event failed its integrity check and was not sent. Stage it again."
            }
            CalError::UnknownCreate => {
                "The staged event is no longer available. Stage it again before allowing it."
            }
            CalError::InvalidRange => {
                "The event range is invalid. Use ordered UTC epochs spanning at most 31 days."
            }
            CalError::InvalidDraft(_) => {
                "The event draft is invalid. Check its path, title, description, and start/end epochs."
            }
            CalError::InvalidArgument(_) => {
                "The calendar connection or path is invalid. Check the explicit fields and try again."
            }
            CalError::Malformed(_) => {
                "The provider returned malformed calendar data. Nothing was cached from this response."
            }
            CalError::TooLarge => {
                "The provider response exceeded the safety limit. Nothing was cached from it."
            }
            CalError::UnsupportedTimezone(_) => {
                "The event uses an unsupported timezone. Use UTC epochs and try again."
            }
            CalError::Discovery(_) => {
                "Calendar discovery failed. Check the HTTPS base path and provider access, then reconnect."
            }
            CalError::TlsRequired => {
                "The provider refused a non-TLS connection. Use an HTTPS host and port."
            }
            CalError::Transport(_) => {
                "The calendar provider could not be reached. Check the HTTPS host/port and try again."
            }
            CalError::Http(_) => {
                "The provider returned an unexpected HTTP response. Check the account and base path."
            }
        }
    }
}

#[cfg(not(calendar_controller_only))]
#[allow(clippy::too_many_lines)]
mod native {
    use super::{
        CalendarController, MAX_EVENT_RANGE_SECS, Operation, ReplyDisposition, RequestTicket,
    };
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_calendar::{
        CalendarAccount, CalendarClient, CalendarDraft, CalendarInfo, CreateReceipt, EventSummary,
        PendingCreate,
    };
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    pub struct CalendarPage {
        pub root: gtk::Box,
        pub shutdown: Rc<dyn Fn()>,
        #[cfg(test)]
        #[allow(dead_code)]
        pub(super) ui: Rc<Ui>,
    }

    #[derive(Clone)]
    struct Snapshot {
        account: CalendarAccount,
        password: String,
        generation: u64,
    }

    enum Reply {
        Calendars(
            RequestTicket,
            Result<Vec<CalendarInfo>, muse_calendar::CalError>,
        ),
        Events(
            RequestTicket,
            Result<Vec<EventSummary>, muse_calendar::CalError>,
        ),
        Approved(
            RequestTicket,
            Result<CreateReceipt, muse_calendar::CalError>,
        ),
    }

    pub(super) struct Ui {
        controller: RefCell<CalendarController>,
        client: Arc<CalendarClient>,
        connection_generation: Cell<u64>,
        closed: Cell<bool>,
        snapshot: RefCell<Option<Snapshot>>,
        pending: RefCell<Option<PendingCreate>>,
        calendars: RefCell<Vec<CalendarInfo>>,
        events: RefCell<Vec<EventSummary>>,
        host: gtk::Entry,
        port: gtk::Entry,
        username: gtk::Entry,
        password: gtk::Entry,
        base_path: gtk::Entry,
        connection_notice: gtk::Label,
        calendar_combo: gtk::ComboBoxText,
        from_epoch: gtk::Entry,
        to_epoch: gtk::Entry,
        events_notice: gtk::Label,
        event_list: gtk::ListBox,
        create_calendar_path: gtk::Entry,
        create_title: gtk::Entry,
        create_start_epoch: gtk::Entry,
        create_end_epoch: gtk::Entry,
        create_description: gtk::TextView,
        stage_notice: gtk::Label,
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

    fn clear_list(list: &gtk::ListBox) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
    }

    fn parse_epoch(entry: &gtk::Entry, message: &'static str) -> Result<i64, &'static str> {
        entry.text().trim().parse().map_err(|_| message)
    }

    fn render_draft_preview(draft: &CalendarDraft) -> String {
        format!(
            "Calendar path: {}\nTitle: {}\nStart UTC epoch: {}\nEnd UTC epoch: {}\nDescription:\n{}",
            draft.calendar_path, draft.title, draft.start_utc, draft.end_utc, draft.description
        )
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            self.closed.set(true);
            self.controller.get_mut().invalidate();
        }
    }

    impl Ui {
        fn discard_staged(&self) {
            let staged_id = self.controller.borrow_mut().clear_stage();
            if let Some(pending) = self.pending.borrow_mut().take() {
                let _ = self.client.deny_create(&pending);
                debug_assert_eq!(staged_id, Some(pending.id));
            }
            self.approval_card.set_visible(false);
            set_text(&self.preview, "");
        }

        fn invalidate_connection(&self) {
            self.connection_generation
                .set(self.connection_generation.get().wrapping_add(1));
            self.controller.borrow_mut().invalidate();
            self.discard_staged();
            self.snapshot.replace(None);
            self.calendars.replace(Vec::new());
            self.events.replace(Vec::new());
            self.calendar_combo.remove_all();
            clear_list(&self.event_list);
        }

        fn shutdown(&self) {
            if self.closed.replace(true) {
                return;
            }
            self.invalidate_connection();
            self.password.set_text("");
        }

        fn connection_fields_changed(&self) {
            if !self.closed.get() && !self.controller.borrow().is_busy() {
                self.invalidate_connection();
                self.connection_notice.set_text(
                    "Connection fields changed. Reconnect and rediscover calendars before reading or approving.",
                );
            }
        }

        fn snapshot(&self) -> Result<Snapshot, &'static str> {
            let host = self.host.text().trim().to_string();
            if host.is_empty()
                || host.starts_with("http://")
                || host.starts_with("https://")
                || host.contains([' ', '\r', '\n', '/', '@', '?', '#'])
            {
                return Err("Enter the HTTPS host without a scheme, path, username, or password.");
            }
            let port = self
                .port
                .text()
                .trim()
                .parse::<u16>()
                .map_err(|_| "HTTPS port must be a number from 1 to 65535.")?;
            if port == 0 {
                return Err("HTTPS port must not be zero.");
            }
            let username = self.username.text().trim().to_string();
            if username.is_empty() || username.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
                return Err("Enter a username without control characters.");
            }
            let password = self.password.text().to_string();
            if password.is_empty() {
                return Err("Enter the password; it stays in memory only and is never stored.");
            }
            let base_path = self.base_path.text().trim().to_string();
            if !base_path.starts_with('/')
                || base_path.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
                || base_path.contains(['?', '#'])
            {
                return Err(
                    "Base path must be an absolute provider path without query or fragment text.",
                );
            }
            Ok(Snapshot {
                account: CalendarAccount {
                    host,
                    port,
                    username,
                    base_path,
                },
                password,
                generation: self.connection_generation.get(),
            })
        }

        fn selected_calendar_path(&self) -> Option<String> {
            let index = self.calendar_combo.active()? as usize;
            self.calendars
                .borrow()
                .get(index)
                .map(|calendar| calendar.path.clone())
        }

        fn event_range(&self) -> Result<(i64, i64), &'static str> {
            let from = parse_epoch(&self.from_epoch, "From UTC epoch must be a signed integer.")?;
            let to = parse_epoch(&self.to_epoch, "To UTC epoch must be a signed integer.")?;
            let span = to
                .checked_sub(from)
                .ok_or("UTC epoch range overflowed. Enter a smaller ordered range.")?;
            if span <= 0 || span > MAX_EVENT_RANGE_SECS {
                return Err("UTC event range must be ordered and span at most 31 days.");
            }
            Ok((from, to))
        }

        fn set_busy(&self, busy: bool) {
            self.host.set_sensitive(!busy);
            self.port.set_sensitive(!busy);
            self.username.set_sensitive(!busy);
            self.password.set_sensitive(!busy);
            self.base_path.set_sensitive(!busy);
            if busy {
                self.spinner.start();
            } else {
                self.spinner.stop();
            }
        }

        fn begin(&self, operation: Operation, notice: &gtk::Label) -> Option<RequestTicket> {
            if self.closed.get() {
                return None;
            }
            match self.controller.borrow_mut().begin(operation) {
                Ok(ticket) => {
                    self.set_busy(true);
                    Some(ticket)
                }
                Err(message) => {
                    notice.set_text(message);
                    None
                }
            }
        }

        fn finish(&self, ticket: RequestTicket) -> bool {
            matches!(
                self.controller.borrow_mut().finish(ticket),
                ReplyDisposition::Applied
            )
        }

        fn connect_calendars(&self, ui: &Rc<Ui>) {
            if self.controller.borrow().is_busy() {
                self.connection_notice.set_text(
                    "A calendar request is already running. Wait for it to finish first.",
                );
                return;
            }
            self.invalidate_connection();
            let snapshot = match self.snapshot() {
                Ok(snapshot) => snapshot,
                Err(message) => {
                    self.connection_notice.set_text(message);
                    return;
                }
            };
            let ticket = match self.begin(Operation::Discover, &self.connection_notice) {
                Some(ticket) => ticket,
                None => return,
            };
            self.calendar_combo.remove_all();
            self.calendars.replace(Vec::new());
            self.events.replace(Vec::new());
            clear_list(&self.event_list);
            self.connection_notice
                .set_text("Connecting over HTTPS and discovering calendars…");
            let client = Arc::clone(&self.client);
            let snapshot_for_ui = snapshot.clone();
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = client.calendars(&account, &password);
                let _ = tx.send(Reply::Calendars(ticket, result));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Calendars(reply_ticket, result)) => {
                        if !ui.finish(reply_ticket) {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(calendars) => {
                                ui.snapshot.replace(Some(snapshot_for_ui.clone()));
                                ui.calendar_combo.remove_all();
                                for calendar in &calendars {
                                    ui.calendar_combo.append_text(&format!(
                                        "{} — {}",
                                        calendar.name, calendar.path
                                    ));
                                }
                                if !calendars.is_empty() {
                                    ui.calendar_combo.set_active(Some(0));
                                    ui.create_calendar_path.set_text(&calendars[0].path);
                                    ui.connection_notice.set_text(
                                        "Connected. Select a calendar and press Refresh events; nothing polls in the background.",
                                    );
                                } else {
                                    ui.connection_notice.set_text(
                                        "Connected, but the provider returned no calendars. Check the base path or account access.",
                                    );
                                }
                                ui.calendars.replace(calendars);
                            }
                            Err(error) => {
                                ui.connection_notice
                                    .set_text(CalendarController::notice_for_error(&error));
                            }
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.finish(ticket) {
                            ui.set_busy(false);
                            ui.connection_notice.set_text(
                                "Calendar discovery worker stopped unexpectedly. Nothing was cached.",
                            );
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(Reply::Events(_, _)) | Ok(Reply::Approved(_, _)) => {
                        gtk::glib::ControlFlow::Continue
                    }
                }
            });
        }

        fn refresh_events(&self, ui: &Rc<Ui>) {
            if self.controller.borrow().is_busy() {
                self.events_notice.set_text(
                    "A calendar request is already running. Wait for it to finish first.",
                );
                return;
            }
            let Some(snapshot) = self.snapshot.borrow().clone() else {
                self.events_notice
                    .set_text("Not connected. Press Connect / discover calendars first.");
                return;
            };
            let Some(calendar_path) = self.selected_calendar_path() else {
                self.events_notice
                    .set_text("Select a discovered calendar before refreshing events.");
                return;
            };
            let (from, to) = match self.event_range() {
                Ok(range) => range,
                Err(message) => {
                    self.events_notice.set_text(message);
                    return;
                }
            };
            let ticket = match self.begin(Operation::Events, &self.events_notice) {
                Some(ticket) => ticket,
                None => return,
            };
            self.events_notice
                .set_text("Refreshing the selected calendar for the explicit UTC range…");
            let client = Arc::clone(&self.client);
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = client.events(&account, &password, &calendar_path, from, to);
                let _ = tx.send(Reply::Events(ticket, result));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Events(reply_ticket, result)) => {
                        if !ui.finish(reply_ticket) {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(events) => {
                                clear_list(&ui.event_list);
                                for event in &events {
                                    let row = label(&format!(
                                        "{} · UTC {}–{}{}",
                                        event.title,
                                        event.start_utc,
                                        event.end_utc,
                                        if event.all_day { " · all day" } else { "" }
                                    ));
                                    ui.event_list.append(&row);
                                }
                                ui.events.replace(events);
                                ui.events_notice.set_text(
                                    "Events loaded for the explicit range. Refresh is always manual; nothing polls.",
                                );
                            }
                            Err(error) => {
                                ui.events_notice
                                    .set_text(CalendarController::notice_for_error(&error));
                            }
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.finish(ticket) {
                            ui.set_busy(false);
                            ui.events_notice.set_text(
                                "Events worker stopped unexpectedly. The list is unchanged.",
                            );
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(Reply::Calendars(_, _)) | Ok(Reply::Approved(_, _)) => {
                        gtk::glib::ControlFlow::Continue
                    }
                }
            });
        }

        fn stage_event(&self) {
            let start = match parse_epoch(
                &self.create_start_epoch,
                "Start UTC epoch must be a signed integer.",
            ) {
                Ok(value) => value,
                Err(message) => {
                    self.stage_notice.set_text(message);
                    return;
                }
            };
            let end = match parse_epoch(
                &self.create_end_epoch,
                "End UTC epoch must be a signed integer.",
            ) {
                Ok(value) => value,
                Err(message) => {
                    self.stage_notice.set_text(message);
                    return;
                }
            };
            let draft = CalendarDraft {
                calendar_path: self.create_calendar_path.text().to_string(),
                title: self.create_title.text().to_string(),
                start_utc: start,
                end_utc: end,
                description: text_of(&self.create_description),
            };
            let pending = match self.client.queue_create(draft.clone()) {
                Ok(pending) => pending,
                Err(error) => {
                    if self.controller.borrow().preview().is_some() {
                        self.stage_notice.set_text(
                            "Draft rejected. The existing staged event remains awaiting your decision.",
                        );
                    } else {
                        self.stage_notice
                            .set_text(CalendarController::notice_for_error(&error));
                    }
                    return;
                }
            };
            let preview = render_draft_preview(&draft);
            let old_id = self.controller.borrow_mut().stage(
                pending.id,
                preview.clone(),
                self.connection_generation.get(),
            );
            if let Some(old_pending) = self.pending.borrow_mut().replace(pending) {
                // Restaging is local only; remove the old queue entry so only
                // the exact preview currently shown can be approved.
                let _ = self.client.deny_create(&old_pending);
                if old_id != Some(old_pending.id) {
                    self.stage_notice.set_text(
                        "The previous staged event was discarded before this preview was shown.",
                    );
                }
            }
            set_text(&self.preview, &preview);
            self.approval_card.set_visible(true);
            self.stage_notice.set_text(
                "Event staged locally. Review the exact fields below; Allow once is required and is never retried.",
            );
        }

        fn allow_once(&self, ui: &Rc<Ui>) {
            if self.controller.borrow().is_busy() {
                self.stage_notice.set_text(
                    "A calendar request is already running. Wait for it to finish first.",
                );
                return;
            }
            let Some(snapshot) = self.snapshot.borrow().clone() else {
                self.stage_notice
                    .set_text("Connect first; Allow once needs the live account fields.");
                return;
            };
            let generation = snapshot.generation;
            let ticket = match self.begin(Operation::Approve, &self.stage_notice) {
                Some(ticket) => ticket,
                None => return,
            };
            let Some(staged_id) = self.controller.borrow_mut().take_for_allow(generation) else {
                if let Some(stale_id) = self.controller.borrow_mut().clear_stage() {
                    if let Some(stale_pending) = self.pending.borrow_mut().take() {
                        let _ = self.client.deny_create(&stale_pending);
                        debug_assert_eq!(Some(stale_pending.id), Some(stale_id));
                    }
                    let _ = self.controller.borrow_mut().finish(ticket);
                    self.set_busy(false);
                    self.stage_notice.set_text(
                        "The connection changed after staging. The event was discarded; reconnect and stage it again.",
                    );
                    return;
                }
                let _ = self.controller.borrow_mut().finish(ticket);
                self.set_busy(false);
                self.stage_notice
                    .set_text("Nothing is staged to allow. Stage an event first.");
                return;
            };
            let Some(pending) = self.pending.borrow_mut().take() else {
                let _ = self.controller.borrow_mut().finish(ticket);
                self.set_busy(false);
                self.stage_notice
                    .set_text("The staged event token is missing. Stage the event again.");
                return;
            };
            if pending.id != staged_id {
                let _ = self.client.deny_create(&pending);
                let _ = self.controller.borrow_mut().finish(ticket);
                self.set_busy(false);
                self.stage_notice.set_text(
                    "The staged event token did not match its preview. Nothing was sent; stage again.",
                );
                return;
            }
            self.approval_card.set_visible(false);
            set_text(&self.preview, "");
            self.stage_notice.set_text(
                "Allow once is in progress. The staged token is consumed and will not be retried…",
            );
            let client = Arc::clone(&self.client);
            let account = snapshot.account.clone();
            let password = snapshot.password.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = client.approve_create(&account, &password, &pending);
                let _ = tx.send(Reply::Approved(ticket, result));
            });
            let weak = Rc::downgrade(ui);
            gtk::glib::timeout_add_local(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(Reply::Approved(reply_ticket, result)) => {
                        if !ui.finish(reply_ticket) {
                            return gtk::glib::ControlFlow::Break;
                        }
                        ui.set_busy(false);
                        match result {
                            Ok(_) => ui.stage_notice.set_text(
                                "Event created. The provider acknowledged this one approval; nothing was retried.",
                            ),
                            Err(error) => ui
                                .stage_notice
                                .set_text(CalendarController::notice_for_error(&error)),
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if ui.finish(ticket) {
                            ui.set_busy(false);
                            ui.stage_notice.set_text(
                                "Create outcome is uncertain because the approval worker stopped. Do not retry automatically; verify the provider first.",
                            );
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(Reply::Calendars(_, _)) | Ok(Reply::Events(_, _)) => {
                        gtk::glib::ControlFlow::Continue
                    }
                }
            });
        }

        fn deny(&self) {
            let Some(staged_id) = self.controller.borrow_mut().deny_staged() else {
                self.stage_notice
                    .set_text("Nothing is staged to deny; no server request was made.");
                return;
            };
            if let Some(pending) = self.pending.borrow_mut().take() {
                let _ = self.client.deny_create(&pending);
                if pending.id != staged_id {
                    self.stage_notice.set_text(
                        "The staged event token did not match its preview; local state was discarded and no server request was made.",
                    );
                    self.approval_card.set_visible(false);
                    set_text(&self.preview, "");
                    return;
                }
            }
            self.approval_card.set_visible(false);
            set_text(&self.preview, "");
            self.stage_notice
                .set_text("Staged event denied. No server request was made.");
        }
    }

    pub fn build() -> CalendarPage {
        const _: () = super::MAX_EVENT_RANGE_SECS_PIN;

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label(
            "Calendar · generic CalDAV read and approval-gated create",
        ));

        let settings = gtk::Expander::new(Some("CalDAV connection (HTTPS, no OAuth)"));
        settings.add_css_class("settings-disclosure");
        settings.set_expanded(true);
        let settings_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        settings_content.add_css_class("settings-content");
        settings_content.append(&label(
            "Enter the provider explicitly. Passwords stay in memory only and are passed per operation; this page never polls in the background.",
        ));

        let host = gtk::Entry::new();
        host.set_placeholder_text(Some("HTTPS host, without https://"));
        settings_content.append(&label("HTTPS host"));
        settings_content.append(&host);
        let port = gtk::Entry::new();
        port.set_text("443");
        port.set_placeholder_text(Some("HTTPS port"));
        settings_content.append(&label("HTTPS port"));
        settings_content.append(&port);
        let username = gtk::Entry::new();
        username.set_placeholder_text(Some("Username"));
        settings_content.append(&label("Username"));
        settings_content.append(&username);
        let password = gtk::Entry::new();
        password.set_visibility(false);
        password.set_placeholder_text(Some("Password (memory only)"));
        settings_content.append(&label("Password"));
        settings_content.append(&password);
        let base_path = gtk::Entry::new();
        base_path.set_text("/");
        base_path.set_placeholder_text(Some("Absolute CalDAV base path"));
        settings_content.append(&label("Base path"));
        settings_content.append(&base_path);

        let connection_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        connection_actions.add_css_class("toolbar");
        let connect = gtk::Button::with_label("Connect / discover calendars");
        connect.add_css_class("suggested-action");
        connection_actions.append(&connect);
        settings_content.append(&connection_actions);
        let connection_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        connection_row.append(&spinner);
        let connection_notice =
            label("Not connected. Enter provider details and press Connect / discover calendars.");
        connection_notice.set_hexpand(true);
        connection_row.append(&connection_notice);
        settings_content.append(&connection_row);
        settings.set_child(Some(&settings_content));
        root.append(&settings);

        let calendar_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        calendar_row.add_css_class("toolbar");
        let calendar_combo = gtk::ComboBoxText::new();
        calendar_combo.set_hexpand(true);
        calendar_row.append(&calendar_combo);
        let refresh = gtk::Button::with_label("Refresh events");
        calendar_row.append(&refresh);
        root.append(&calendar_row);

        let range_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let from_epoch = gtk::Entry::new();
        from_epoch.set_hexpand(true);
        from_epoch.set_placeholder_text(Some("From UTC epoch"));
        let to_epoch = gtk::Entry::new();
        to_epoch.set_hexpand(true);
        to_epoch.set_placeholder_text(Some("To UTC epoch (max 31 days)"));
        range_row.append(&label("Event range:"));
        range_row.append(&from_epoch);
        range_row.append(&to_epoch);
        root.append(&range_row);
        let events_notice =
            label("No events loaded. Enter an explicit UTC epoch range and press Refresh events.");
        root.append(&events_notice);
        let event_list = gtk::ListBox::new();
        event_list.set_selection_mode(gtk::SelectionMode::None);
        let event_scroll = gtk::ScrolledWindow::new();
        event_scroll.set_min_content_height(120);
        event_scroll.set_child(Some(&event_list));
        root.append(&event_scroll);

        let create_heading = label("Create event (local stage, then one fresh approval)");
        create_heading.add_css_class("section-heading");
        root.append(&create_heading);
        let create_calendar_path = gtk::Entry::new();
        create_calendar_path.set_placeholder_text(Some("Absolute calendar path"));
        root.append(&label("Calendar path"));
        root.append(&create_calendar_path);
        let create_title = gtk::Entry::new();
        create_title.set_placeholder_text(Some("Event title"));
        root.append(&label("Title"));
        root.append(&create_title);
        let create_start_epoch = gtk::Entry::new();
        create_start_epoch.set_placeholder_text(Some("Start UTC epoch"));
        root.append(&label("Start UTC epoch"));
        root.append(&create_start_epoch);
        let create_end_epoch = gtk::Entry::new();
        create_end_epoch.set_placeholder_text(Some("End UTC epoch (max 24 hours)"));
        root.append(&label("End UTC epoch"));
        root.append(&create_end_epoch);
        let create_description = gtk::TextView::new();
        create_description.set_wrap_mode(gtk::WrapMode::WordChar);
        create_description.set_vexpand(false);
        create_description.set_size_request(-1, 76);
        root.append(&label("Description"));
        root.append(&create_description);
        let stage = gtk::Button::with_label("Stage event");
        stage.add_css_class("suggested-action");
        root.append(&stage);
        let stage_notice = label(
            "No event staged. Staging sends nothing; Allow once is required for a server write.",
        );
        root.append(&stage_notice);

        let approval_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        approval_card.add_css_class("card");
        approval_card.add_css_class("approval-card");
        approval_card.append(&label(
            "Allow once — exact staged event fields below; no overwrite and no automatic retry",
        ));
        let preview = gtk::TextView::new();
        preview.set_editable(false);
        preview.set_cursor_visible(false);
        preview.set_wrap_mode(gtk::WrapMode::WordChar);
        let preview_scroll = gtk::ScrolledWindow::new();
        preview_scroll.set_min_content_height(120);
        preview_scroll.set_child(Some(&preview));
        preview_scroll.add_css_class("approval-details");
        approval_card.append(&preview_scroll);
        let approval_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let allow = gtk::Button::with_label("Allow once");
        allow.add_css_class("suggested-action");
        let deny = gtk::Button::with_label("Deny");
        deny.add_css_class("destructive-action");
        approval_actions.append(&allow);
        approval_actions.append(&deny);
        approval_card.append(&approval_actions);
        approval_card.set_visible(false);
        root.append(&approval_card);

        let connection_entries = [
            host.clone(),
            port.clone(),
            username.clone(),
            password.clone(),
            base_path.clone(),
        ];
        let ui = Rc::new(Ui {
            controller: RefCell::new(CalendarController::new()),
            client: Arc::new(CalendarClient::new()),
            connection_generation: Cell::new(0),
            closed: Cell::new(false),
            snapshot: RefCell::new(None),
            pending: RefCell::new(None),
            calendars: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
            host,
            port,
            username,
            password,
            base_path,
            connection_notice,
            calendar_combo,
            from_epoch,
            to_epoch,
            events_notice,
            event_list,
            create_calendar_path,
            create_title,
            create_start_epoch,
            create_end_epoch,
            create_description,
            stage_notice,
            approval_card,
            preview,
            spinner,
        });

        {
            let weak = Rc::downgrade(&ui);
            connect.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.connect_calendars(&ui);
                }
            });
        }
        {
            let weak = Rc::downgrade(&ui);
            refresh.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.refresh_events(&ui);
                }
            });
        }
        {
            let weak = Rc::downgrade(&ui);
            stage.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.stage_event();
                }
            });
        }
        {
            let weak = Rc::downgrade(&ui);
            allow.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.allow_once(&ui);
                }
            });
        }
        {
            let weak = Rc::downgrade(&ui);
            deny.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.deny();
                }
            });
        }

        for entry in connection_entries {
            let weak = Rc::downgrade(&ui);
            entry.connect_changed(move |_| {
                if let Some(ui) = weak.upgrade() {
                    ui.connection_fields_changed();
                }
            });
        }

        let shutdown_ui = Rc::clone(&ui);
        let shutdown: Rc<dyn Fn()> = Rc::new(move || shutdown_ui.shutdown());

        CalendarPage {
            root,
            shutdown,
            #[cfg(test)]
            ui,
        }
    }
}

#[cfg(not(calendar_controller_only))]
pub use native::{CalendarPage, build};

#[cfg(test)]
mod tests {
    use super::{CalendarController, Operation, ReplyDisposition};
    use muse_calendar::CalError;

    #[test]
    fn busy_rejection_blocks_a_second_network_operation() {
        let mut controller = CalendarController::new();
        controller
            .begin(Operation::Discover)
            .expect("first operation starts");

        let error = controller
            .begin(Operation::Events)
            .expect_err("busy controller must reject concurrent work");
        assert!(error.contains("already running"));
    }

    #[test]
    fn stale_epoch_reply_is_suppressed() {
        let mut controller = CalendarController::new();
        let ticket = controller
            .begin(Operation::Discover)
            .expect("operation starts");
        controller.invalidate();

        assert_eq!(controller.finish(ticket), ReplyDisposition::Stale);
        assert!(!controller.is_busy());
    }

    #[test]
    fn staging_keeps_exact_preview_and_allow_is_fresh_and_once_only() {
        let mut controller = CalendarController::new();
        let first_preview = "Calendar path: /work\nTitle: Planning\nStart UTC epoch: 10\nEnd UTC epoch: 20\nDescription:\nExact text";
        let second_preview = "Calendar path: /work\nTitle: Review\nStart UTC epoch: 30\nEnd UTC epoch: 40\nDescription:\nNew exact text";

        assert_eq!(controller.stage(41, first_preview.into(), 1), None);
        assert_eq!(controller.preview(), Some(first_preview));
        assert_eq!(controller.stage(42, second_preview.into(), 1), Some(41));
        assert_eq!(controller.preview(), Some(second_preview));
        assert_eq!(controller.take_for_allow(2), None);
        assert_eq!(controller.preview(), Some(second_preview));
        assert_eq!(controller.take_for_allow(1), Some(42));
        assert_eq!(controller.take_for_allow(1), None);
        assert_eq!(controller.preview(), None);
    }

    #[test]
    fn connection_invalidation_clears_staged_approval() {
        let mut controller = CalendarController::new();
        controller.stage(73, "exact preview".into(), 4);

        assert_eq!(controller.clear_stage(), Some(73));
        assert_eq!(controller.preview(), None);
        assert_eq!(controller.take_for_allow(4), None);
        assert!(!controller.is_busy());
    }

    #[test]
    fn deny_discards_stage_without_starting_network_work() {
        let mut controller = CalendarController::new();
        controller.stage(73, "exact preview".into(), 1);

        assert_eq!(controller.deny_staged(), Some(73));
        assert_eq!(controller.take_for_allow(1), None);
        assert!(!controller.is_busy());
    }

    #[test]
    fn provider_failures_have_actionable_sanitized_notices() {
        let cases = [
            (CalError::Conflict, "not overwritten"),
            (CalError::Uncertain, "Do not retry"),
            (CalError::Auth, "username/password"),
            (
                CalError::Transport("private host detail".into()),
                "provider",
            ),
        ];

        for (error, expected) in cases {
            let notice = CalendarController::notice_for_error(&error);
            assert!(notice.contains(expected), "notice was: {notice}");
            assert!(!notice.contains("private host detail"));
        }
    }
}
