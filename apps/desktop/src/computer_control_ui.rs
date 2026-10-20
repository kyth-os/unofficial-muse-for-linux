//! Native GTK Computer Control surface.
//!
//! Portal I/O is isolated to one worker thread. The GTK side only stages
//! actions through the pure policy broker and sends already-approved actions.

#![allow(unexpected_cfgs)]

use crate::computer_control::Action;

#[derive(Clone, Copy, Debug, PartialEq)]
enum MappedAction {
    PointerMotion { dx: f64, dy: f64 },
    PointerButton { button: u16, state: u8 },
    PointerAxis { dx: f64, dy: f64 },
    PointerAxisDiscrete { axis: u32, steps: i32 },
    KeyboardKeysym { keysym: u32, state: u8 },
    KeyboardKeycode { keycode: u16, state: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MappingError {
    ScreenshotUnsupported,
    DiscreteScrollBothAxes,
    DiscreteScrollZero,
}

fn input_dispatch_allowed(cancelled: bool) -> bool {
    !cancelled
}

fn map_approved_action(action: &Action) -> Result<MappedAction, MappingError> {
    match action {
        Action::Screenshot { .. } => Err(MappingError::ScreenshotUnsupported),
        Action::RelativePointerMotion { dx, dy } => {
            Ok(MappedAction::PointerMotion { dx: *dx, dy: *dy })
        }
        Action::PointerButton { button, state } => Ok(MappedAction::PointerButton {
            button: *button,
            state: *state,
        }),
        Action::SmoothScroll { dx, dy } => Ok(MappedAction::PointerAxis { dx: *dx, dy: *dy }),
        Action::DiscreteScroll { dx, dy } => match (*dx, *dy) {
            (0, 0) => Err(MappingError::DiscreteScrollZero),
            (x, 0) => Ok(MappedAction::PointerAxisDiscrete { axis: 0, steps: x }),
            (0, y) => Ok(MappedAction::PointerAxisDiscrete { axis: 1, steps: y }),
            (_, _) => Err(MappingError::DiscreteScrollBothAxes),
        },
        Action::KeyboardKeysym { keysym, state } => Ok(MappedAction::KeyboardKeysym {
            keysym: *keysym,
            state: *state,
        }),
        Action::KeyboardKeycode { keycode, state } => Ok(MappedAction::KeyboardKeycode {
            keycode: *keycode,
            state: *state,
        }),
    }
}

fn mapping_notice(error: MappingError) -> &'static str {
    match error {
        MappingError::ScreenshotUnsupported => {
            "Screenshot is handled only by the existing Desktop page. No input call was made."
        }
        MappingError::DiscreteScrollBothAxes => {
            "Discrete scroll must use exactly one nonzero axis. No input call was made."
        }
        MappingError::DiscreteScrollZero => {
            "Discrete scroll must use one nonzero axis. No input call was made."
        }
    }
}

#[cfg(not(desktop_controller_only))]
mod native {
    use super::{
        Action, MappedAction, input_dispatch_allowed, map_approved_action, mapping_notice,
    };
    use crate::computer_control::{
        ActionName, ApprovedAction, Broker, RequestReceipt, SessionState,
    };
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_desktop::{
        BusEndpoint, DesktopError, DesktopPortal, REMOTE_DEVICE_KEYBOARD, REMOTE_DEVICE_POINTER,
        RemoteDesktopSession,
    };
    use std::cell::{Cell, RefCell};
    use std::fs::File;
    use std::io::Read;
    use std::rc::{Rc, Weak};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    #[derive(Debug)]
    enum WorkerCommand {
        Approved(ApprovedAction),
        Stop,
    }

    enum WorkerReply {
        Ready {
            epoch: u64,
            granted_devices: u32,
        },
        Failed {
            epoch: u64,
            message: &'static str,
        },
        ActionFinished {
            epoch: u64,
            action: ActionName,
            result: Result<(), &'static str>,
        },
        Stopped {
            epoch: u64,
        },
    }

    fn portal_error_kind(error: &DesktopError) -> &'static str {
        match error {
            DesktopError::PermissionDenied => "permission denied",
            DesktopError::Cancelled => "cancelled",
            DesktopError::UnsupportedPortal(_) => "unsupported portal",
            DesktopError::TooLarge => "payload too large",
            DesktopError::Transport(_) => "transport failure",
            DesktopError::InvalidArgument(_) => "invalid argument",
        }
    }

    fn dispatch_action(
        session: &mut RemoteDesktopSession,
        action: MappedAction,
        dispatch_stopped: &AtomicBool,
        dispatch_gate: &Mutex<()>,
    ) -> Result<(), &'static str> {
        let _gate = dispatch_gate
            .lock()
            .map_err(|_| "input dispatch gate unavailable")?;
        if !input_dispatch_allowed(dispatch_stopped.load(Ordering::Acquire)) {
            return Err("cancelled before input dispatch");
        }
        let result = match action {
            MappedAction::PointerMotion { dx, dy } => session.pointer_motion(dx, dy),
            MappedAction::PointerButton { button, state } => {
                session.pointer_button(i32::from(button), u32::from(state))
            }
            MappedAction::PointerAxis { dx, dy } => session.pointer_axis(dx, dy, true),
            MappedAction::PointerAxisDiscrete { axis, steps } => {
                session.pointer_axis_discrete(axis, steps)
            }
            MappedAction::KeyboardKeysym { keysym, state } => {
                session.keyboard_keysym(i32::try_from(keysym).unwrap_or(i32::MAX), u32::from(state))
            }
            MappedAction::KeyboardKeycode { keycode, state } => {
                session.keyboard_keycode(i32::from(keycode), u32::from(state))
            }
        };
        result.map_err(|error| portal_error_kind(&error))
    }

    #[allow(clippy::too_many_arguments)]
    fn worker_loop(
        commands: mpsc::Receiver<WorkerCommand>,
        replies: mpsc::Sender<WorkerReply>,
        bus: BusEndpoint,
        handle_token: String,
        session_token: String,
        parent_window: String,
        requested_devices: u32,
        epoch: u64,
        cancelled: Arc<AtomicBool>,
        dispatch_stopped: Arc<AtomicBool>,
        dispatch_gate: Arc<Mutex<()>>,
    ) {
        let mut session = match DesktopPortal::start_remote_desktop_via_with_cancel(
            &bus,
            &handle_token,
            &session_token,
            &parent_window,
            requested_devices,
            &|| cancelled.load(Ordering::Acquire),
        ) {
            Ok(session) => session,
            Err(error) => {
                let _ = replies.send(WorkerReply::Failed {
                    epoch,
                    message: portal_error_kind(&error),
                });
                return;
            }
        };

        if replies
            .send(WorkerReply::Ready {
                epoch,
                granted_devices: session.granted_devices(),
            })
            .is_err()
        {
            let _ = session.stop();
            return;
        }

        while let Ok(command) = commands.recv() {
            match command {
                WorkerCommand::Approved(approved) => {
                    let action_name = approved.activity_event().action;
                    let result = map_approved_action(&approved.action)
                        .map_err(mapping_notice)
                        .and_then(|mapped| {
                            dispatch_action(&mut session, mapped, &dispatch_stopped, &dispatch_gate)
                        });
                    if replies
                        .send(WorkerReply::ActionFinished {
                            epoch,
                            action: action_name,
                            result,
                        })
                        .is_err()
                    {
                        let _ = session.stop();
                        return;
                    }
                }
                WorkerCommand::Stop => {
                    let _ = session.stop();
                    let _ = replies.send(WorkerReply::Stopped { epoch });
                    return;
                }
            }
        }

        let _ = session.stop();
    }

    fn fresh_session_token(prefix: &str) -> Result<String, &'static str> {
        let mut bytes = [0u8; 16];
        let mut source = File::open("/dev/urandom").map_err(|_| "OS randomness unavailable")?;
        source
            .read_exact(&mut bytes)
            .map_err(|_| "OS randomness unavailable")?;
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push_str(&format!("{byte:02x}"));
        }
        Ok(format!("{prefix}_{encoded}"))
    }

    fn default_bus_path() -> String {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|dir| format!("{dir}/bus"))
            .unwrap_or_default()
    }

    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }

    fn bounded_status(text: &str) -> String {
        text.chars()
            .filter(|character| !character.is_control() || *character == '\n')
            .take(240)
            .collect()
    }

    fn action_summary(action: &Action) -> String {
        let summary = match action {
            Action::RelativePointerMotion { dx, dy } => format!("dx={dx:.1}, dy={dy:.1}"),
            Action::PointerButton { button, state } => format!("button={button}, state={state}"),
            Action::SmoothScroll { dx, dy } => format!("dx={dx:.1}, dy={dy:.1}"),
            Action::DiscreteScroll { dx, dy } => format!("dx={dx}, dy={dy}"),
            Action::KeyboardKeysym { keysym, state } => format!("keysym={keysym}, state={state}"),
            Action::KeyboardKeycode { keycode, state } => {
                format!("keycode={keycode}, state={state}")
            }
            Action::Screenshot { max_bytes } => format!("max bytes={max_bytes}"),
        };
        bounded_status(&summary)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum UiPhase {
        Disabled,
        Starting,
        Active,
        Stopping,
    }

    pub struct ComputerControlPage {
        pub root: gtk::Box,
        pub shutdown: Rc<dyn Fn()>,
    }

    struct Ui {
        epoch: Cell<u64>,
        phase: Cell<UiPhase>,
        closed: Cell<bool>,
        broker: RefCell<Broker>,
        worker: RefCell<Option<mpsc::Sender<WorkerCommand>>>,
        cancelled: Arc<AtomicBool>,
        dispatch_stopped: Arc<AtomicBool>,
        dispatch_gate: Arc<Mutex<()>>,
        granted_devices: Cell<u32>,
        bus: gtk::Entry,
        pointer: gtk::CheckButton,
        keyboard: gtk::CheckButton,
        start: gtk::Button,
        stop: gtk::Button,
        status: gtk::Label,
        approval_box: gtk::Box,
        action_buttons: RefCell<Vec<(gtk::Button, u32)>>,
    }

    impl Ui {
        fn requested_devices(&self) -> u32 {
            let mut devices = 0;
            if self.pointer.is_active() {
                devices |= REMOTE_DEVICE_POINTER;
            }
            if self.keyboard.is_active() {
                devices |= REMOTE_DEVICE_KEYBOARD;
            }
            devices
        }

        fn set_status(&self, text: &str) {
            self.status.set_text(&bounded_status(text));
        }

        fn clear_approvals(&self) {
            while let Some(child) = self.approval_box.first_child() {
                self.approval_box.remove(&child);
            }
        }

        fn set_controls(&self) {
            let phase = self.phase.get();
            let running = !matches!(phase, UiPhase::Disabled);
            self.bus.set_sensitive(!running && !self.closed.get());
            self.pointer.set_sensitive(!running && !self.closed.get());
            self.keyboard.set_sensitive(!running && !self.closed.get());
            self.start
                .set_sensitive(phase == UiPhase::Disabled && !self.closed.get());
            self.stop.set_sensitive(running && !self.closed.get());
            let active = phase == UiPhase::Active;
            let granted = self.granted_devices.get();
            for (button, device) in self.action_buttons.borrow().iter() {
                button.set_sensitive(active && granted & *device != 0 && !self.closed.get());
            }
        }

        fn revoke_policy(&self) {
            let mut broker = self.broker.borrow_mut();
            let _ = broker.stop();
            let _ = broker.revoke();
        }

        fn finish_stop(&self, epoch: u64) {
            if self.epoch.get() != epoch {
                return;
            }
            self.worker.borrow_mut().take();
            self.granted_devices.set(0);
            self.phase.set(UiPhase::Disabled);
            self.clear_approvals();
            self.revoke_policy();
            self.set_controls();
            if !self.closed.get() {
                self.set_status("Stopped. Portal input authority was revoked; nothing is reconnecting automatically.");
            }
        }

        fn stop(&self) {
            if self.closed.get() && self.phase.get() == UiPhase::Disabled {
                return;
            }
            match self.phase.get() {
                UiPhase::Disabled => {
                    self.epoch.set(self.epoch.get().wrapping_add(1));
                    self.clear_approvals();
                    self.revoke_policy();
                    self.granted_devices.set(0);
                    self.set_controls();
                }
                UiPhase::Starting | UiPhase::Active => {
                    let epoch = self.epoch.get();
                    self.phase.set(UiPhase::Stopping);
                    self.cancelled.store(true, Ordering::Release);
                    self.granted_devices.set(0);
                    self.clear_approvals();
                    self.set_controls();
                    self.set_status("Stopping…");
                    let sender = self.worker.borrow().as_ref().cloned();
                    if sender.is_none() {
                        self.dispatch_stopped.store(true, Ordering::Release);
                        self.finish_stop(epoch);
                    } else {
                        let dispatch_gate = Arc::clone(&self.dispatch_gate);
                        let dispatch_stopped = Arc::clone(&self.dispatch_stopped);
                        std::thread::spawn(move || {
                            let Ok(_gate) = dispatch_gate.lock() else {
                                return;
                            };
                            dispatch_stopped.store(true, Ordering::Release);
                            if let Some(sender) = sender {
                                let _ = sender.send(WorkerCommand::Stop);
                            }
                        });
                    }
                }
                UiPhase::Stopping => {}
            }
        }

        fn start(self: &Rc<Self>) {
            if self.phase.get() != UiPhase::Disabled || self.closed.get() {
                return;
            }
            let path = self.bus.text().trim().to_string();
            if path.is_empty() || path.len() > 1024 || path.bytes().any(|byte| byte == 0) {
                self.set_status("Enter the session bus socket path explicitly.");
                return;
            }
            let requested_devices = self.requested_devices();
            if requested_devices == 0 {
                self.set_status("Select pointer and/or keyboard before starting.");
                return;
            }
            if self.broker.borrow_mut().start().is_err() {
                self.set_status("A session is already starting or active.");
                return;
            }

            let epoch = self.epoch.get().wrapping_add(1);
            self.epoch.set(epoch);
            self.cancelled.store(false, Ordering::Release);
            self.dispatch_stopped.store(false, Ordering::Release);
            self.phase.set(UiPhase::Starting);
            self.granted_devices.set(0);
            self.set_controls();
            self.set_status("Starting… Approve the RemoteDesktop request in the system dialog.");

            let (commands_tx, commands_rx) = mpsc::channel();
            let (replies_tx, replies_rx) = mpsc::channel();
            let bus = BusEndpoint::unix_socket(&path);
            let handle_token = match fresh_session_token("muse_handle") {
                Ok(token) => token,
                Err(message) => {
                    let _ = self.broker.borrow_mut().stop();
                    let _ = self.broker.borrow_mut().revoke();
                    self.phase.set(UiPhase::Disabled);
                    self.set_controls();
                    self.set_status(message);
                    return;
                }
            };
            let session_token = match fresh_session_token("muse_session") {
                Ok(token) => token,
                Err(message) => {
                    let _ = self.broker.borrow_mut().stop();
                    let _ = self.broker.borrow_mut().revoke();
                    self.phase.set(UiPhase::Disabled);
                    self.set_controls();
                    self.set_status(message);
                    return;
                }
            };
            let parent_window = String::new();
            let cancelled = Arc::clone(&self.cancelled);
            let dispatch_stopped = Arc::clone(&self.dispatch_stopped);
            let dispatch_gate = Arc::clone(&self.dispatch_gate);
            std::thread::spawn(move || {
                worker_loop(
                    commands_rx,
                    replies_tx,
                    bus,
                    handle_token,
                    session_token,
                    parent_window,
                    requested_devices,
                    epoch,
                    cancelled,
                    dispatch_stopped,
                    dispatch_gate,
                );
            });
            self.worker.borrow_mut().replace(commands_tx);
            self.poll(replies_rx, epoch);
        }

        fn poll(self: &Rc<Self>, replies: mpsc::Receiver<WorkerReply>, epoch: u64) {
            let weak_ui = Rc::downgrade(self);
            gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                let mut finished = false;
                loop {
                    match replies.try_recv() {
                        Ok(WorkerReply::Ready {
                            epoch: reply_epoch,
                            granted_devices,
                        }) => {
                            if reply_epoch != ui.epoch.get() || ui.phase.get() != UiPhase::Starting
                            {
                                continue;
                            }
                            if let Ok(SessionState::Active) = ui.broker.borrow_mut().activate() {
                                ui.granted_devices.set(granted_devices);
                                ui.phase.set(UiPhase::Active);
                                ui.set_controls();
                                let pointer = granted_devices & REMOTE_DEVICE_POINTER != 0;
                                let keyboard = granted_devices & REMOTE_DEVICE_KEYBOARD != 0;
                                ui.set_status(&format!(
                                    "Active. Granted devices: {}{}.",
                                    if pointer { "pointer" } else { "" },
                                    if keyboard {
                                        if pointer { ", keyboard" } else { "keyboard" }
                                    } else {
                                        ""
                                    }
                                ));
                            }
                        }
                        Ok(WorkerReply::Failed {
                            epoch: reply_epoch,
                            message,
                        }) => {
                            if ui.phase.get() == UiPhase::Stopping {
                                if reply_epoch == epoch {
                                    ui.worker.borrow_mut().take();
                                    ui.finish_stop(ui.epoch.get());
                                }
                                finished = true;
                                break;
                            }
                            if reply_epoch != ui.epoch.get() {
                                finished = true;
                                break;
                            }
                            ui.worker.borrow_mut().take();
                            ui.phase.set(UiPhase::Disabled);
                            ui.granted_devices.set(0);
                            ui.revoke_policy();
                            ui.clear_approvals();
                            ui.set_controls();
                            ui.set_status(&format!("RemoteDesktop start failed: {message}."));
                            finished = true;
                            break;
                        }
                        Ok(WorkerReply::ActionFinished {
                            epoch: reply_epoch,
                            action,
                            result,
                        }) => {
                            if reply_epoch != ui.epoch.get() || ui.phase.get() != UiPhase::Active {
                                continue;
                            }
                            match result {
                                Ok(()) => ui.set_status(&format!("Approved {action} action sent.")),
                                Err(message) => {
                                    ui.set_status(&format!("{action} action failed: {message}."))
                                }
                            }
                        }
                        Ok(WorkerReply::Stopped { epoch: reply_epoch }) => {
                            if ui.phase.get() == UiPhase::Stopping && reply_epoch == epoch {
                                ui.finish_stop(ui.epoch.get());
                                finished = true;
                                break;
                            }
                            if reply_epoch != ui.epoch.get() {
                                finished = true;
                                break;
                            }
                            ui.finish_stop(reply_epoch);
                            finished = true;
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            if ui.phase.get() == UiPhase::Stopping {
                                ui.worker.borrow_mut().take();
                                ui.finish_stop(ui.epoch.get());
                            } else if ui.epoch.get() == epoch {
                                ui.worker.borrow_mut().take();
                                ui.phase.set(UiPhase::Disabled);
                                ui.granted_devices.set(0);
                                ui.revoke_policy();
                                ui.clear_approvals();
                                ui.set_controls();
                                ui.set_status("RemoteDesktop worker stopped unexpectedly; input authority was revoked.");
                            }
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
        }

        fn required_device(action: &Action) -> Option<u32> {
            match action {
                Action::Screenshot { .. } => None,
                Action::RelativePointerMotion { .. }
                | Action::PointerButton { .. }
                | Action::SmoothScroll { .. }
                | Action::DiscreteScroll { .. } => Some(REMOTE_DEVICE_POINTER),
                Action::KeyboardKeysym { .. } | Action::KeyboardKeycode { .. } => {
                    Some(REMOTE_DEVICE_KEYBOARD)
                }
            }
        }

        fn submit(self: &Rc<Self>, action: Action) {
            if self.phase.get() != UiPhase::Active {
                self.set_status("Start and approve a RemoteDesktop session before testing input.");
                return;
            }
            if let Some(device) = Self::required_device(&action)
                && self.granted_devices.get() & device == 0
            {
                self.set_status("The portal did not grant the device required for this action.");
                return;
            }
            if let Err(error) = map_approved_action(&action) {
                self.set_status(mapping_notice(error));
                return;
            }
            let receipt = match self.broker.borrow_mut().submit(action.clone()) {
                Ok(receipt) => receipt,
                Err(error) => {
                    self.set_status(&format!("Action was not staged: {error:?}."));
                    return;
                }
            };
            self.render_approval(receipt, action);
        }

        fn render_approval(self: &Rc<Self>, receipt: RequestReceipt, action: Action) {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
            card.add_css_class("card");
            card.add_css_class("approval-card");
            let heading = gtk::Label::new(Some(&format!(
                "Approval required · {}",
                receipt.metadata.name
            )));
            heading.set_xalign(0.0);
            heading.add_css_class("approval-heading");
            card.append(&heading);
            let details = gtk::Label::new(Some(&action_summary(&action)));
            details.set_xalign(0.0);
            details.add_css_class("approval-details");
            card.append(&details);
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.add_css_class("approval-actions");
            let allow = gtk::Button::with_label("Allow once");
            allow.add_css_class("suggested-action");
            let deny = gtk::Button::with_label("Deny");
            deny.add_css_class("destructive-action");
            let decision = gtk::Label::new(Some("Pending"));
            decision.add_css_class("approval-countdown");
            buttons.append(&allow);
            buttons.append(&deny);
            buttons.append(&decision);
            card.append(&buttons);
            self.approval_box.append(&card);

            let decided = Rc::new(Cell::new(false));
            let weak_ui = Rc::downgrade(self);
            let allow_button = allow.clone();
            let deny_button = deny.clone();
            let decision_label = decision.clone();
            let decided_for_allow = Rc::clone(&decided);
            allow.connect_clicked(move |_| {
                if decided_for_allow.replace(true) {
                    return;
                }
                allow_button.set_sensitive(false);
                deny_button.set_sensitive(false);
                decision_label.set_text("Allowed once");
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                match ui
                    .broker
                    .borrow_mut()
                    .allow_once(&receipt.request_id, &receipt.approval_token)
                {
                    Ok(approved) => {
                        let Some(sender) = ui.worker.borrow().as_ref().cloned() else {
                            ui.set_status("Worker unavailable; approved action was not sent.");
                            return;
                        };
                        if sender.send(WorkerCommand::Approved(approved)).is_err() {
                            ui.set_status("Worker unavailable; approved action was not sent.");
                        } else {
                            ui.set_status("Allowed once; action queued for the input worker.");
                        }
                    }
                    Err(error) => ui.set_status(&format!("Approval was not accepted: {error:?}.")),
                }
            });

            let weak_ui = Rc::downgrade(self);
            let allow_button = allow.clone();
            let deny_button = deny.clone();
            let decision_label = decision;
            let decided_for_deny = Rc::clone(&decided);
            deny.connect_clicked(move |_| {
                if decided_for_deny.replace(true) {
                    return;
                }
                allow_button.set_sensitive(false);
                deny_button.set_sensitive(false);
                decision_label.set_text("Denied");
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                match ui
                    .broker
                    .borrow_mut()
                    .deny(&receipt.request_id, &receipt.approval_token)
                {
                    Ok(_) => ui.set_status("Denied; no input call was made."),
                    Err(error) => ui.set_status(&format!("Denial was not accepted: {error:?}.")),
                }
            });
        }
    }

    pub fn build() -> ComputerControlPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label("Computer Control · explicit portal input session"));
        root.append(&label(
        "Account sign-in does not grant Linux input access. This page asks the XDG Desktop Portal for a separate RemoteDesktop consent, and every test action needs one fresh Allow once approval.",
    ));

        let settings = gtk::Expander::new(Some("Session bus and requested devices"));
        settings.add_css_class("settings-disclosure");
        settings.set_expanded(true);
        let settings_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        settings_content.add_css_class("settings-content");
        settings_content.append(&label(
        "Use the session bus socket for the desktop portal. No automatic start or reconnect is performed.",
    ));
        let bus = gtk::Entry::new();
        bus.set_text(&default_bus_path());
        bus.set_placeholder_text(Some("$XDG_RUNTIME_DIR/bus"));
        settings_content.append(&bus);
        let pointer = gtk::CheckButton::with_label("Request pointer input");
        pointer.set_active(true);
        let keyboard = gtk::CheckButton::with_label("Request keyboard input");
        settings_content.append(&pointer);
        settings_content.append(&keyboard);
        settings.set_child(Some(&settings_content));
        root.append(&settings);

        let control_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let start = gtk::Button::with_label("Start");
        start.add_css_class("suggested-action");
        let stop = gtk::Button::with_label("Stop");
        stop.add_css_class("destructive-action");
        stop.set_sensitive(false);
        control_row.append(&start);
        control_row.append(&stop);
        root.append(&control_row);

        let status = label("Disabled. No RemoteDesktop session is running.");
        status.add_css_class("chat-status");
        root.append(&status);

        let actions_heading = label("Safe local test actions");
        actions_heading.add_css_class("section-heading");
        root.append(&actions_heading);
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.append(&actions);

        let approval_heading = label("Pending approvals");
        approval_heading.add_css_class("section-heading");
        root.append(&approval_heading);
        let approval_scroll = gtk::ScrolledWindow::new();
        approval_scroll.add_css_class("approval-scroll");
        approval_scroll.set_vexpand(true);
        let approval_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        approval_scroll.set_child(Some(&approval_box));
        root.append(&approval_scroll);

        let ui = Rc::new(Ui {
            epoch: Cell::new(0),
            phase: Cell::new(UiPhase::Disabled),
            closed: Cell::new(false),
            broker: RefCell::new(Broker::new()),
            worker: RefCell::new(None),
            cancelled: Arc::new(AtomicBool::new(false)),
            dispatch_stopped: Arc::new(AtomicBool::new(false)),
            dispatch_gate: Arc::new(Mutex::new(())),
            granted_devices: Cell::new(0),
            bus,
            pointer,
            keyboard,
            start,
            stop,
            status,
            approval_box,
            action_buttons: RefCell::new(Vec::new()),
        });

        let add_action =
            |ui: &Rc<Ui>, actions: &gtk::Box, name: &str, device: u32, action: Action| {
                let button = gtk::Button::with_label(name);
                let weak_ui: Weak<Ui> = Rc::downgrade(ui);
                let action_for_click = action.clone();
                button.connect_clicked(move |_| {
                    if let Some(ui) = weak_ui.upgrade() {
                        ui.submit(action_for_click.clone());
                    }
                });
                actions.append(&button);
                ui.action_buttons.borrow_mut().push((button, device));
            };
        add_action(
            &ui,
            &actions,
            "Pointer motion (+12, −8)",
            REMOTE_DEVICE_POINTER,
            Action::RelativePointerMotion { dx: 12.0, dy: -8.0 },
        );
        add_action(
            &ui,
            &actions,
            "Pointer click press",
            REMOTE_DEVICE_POINTER,
            Action::PointerButton {
                button: 0x110,
                state: 1,
            },
        );
        add_action(
            &ui,
            &actions,
            "Pointer click release",
            REMOTE_DEVICE_POINTER,
            Action::PointerButton {
                button: 0x110,
                state: 0,
            },
        );
        add_action(
            &ui,
            &actions,
            "Scroll up one step",
            REMOTE_DEVICE_POINTER,
            Action::DiscreteScroll { dx: 0, dy: -1 },
        );
        add_action(
            &ui,
            &actions,
            "Scroll down one step",
            REMOTE_DEVICE_POINTER,
            Action::DiscreteScroll { dx: 0, dy: 1 },
        );
        add_action(
            &ui,
            &actions,
            "Keysym ‘a’ press",
            REMOTE_DEVICE_KEYBOARD,
            Action::KeyboardKeysym {
                keysym: 0x61,
                state: 1,
            },
        );
        add_action(
            &ui,
            &actions,
            "Keysym ‘a’ release",
            REMOTE_DEVICE_KEYBOARD,
            Action::KeyboardKeysym {
                keysym: 0x61,
                state: 0,
            },
        );
        add_action(
            &ui,
            &actions,
            "Keycode 30 press",
            REMOTE_DEVICE_KEYBOARD,
            Action::KeyboardKeycode {
                keycode: 30,
                state: 1,
            },
        );
        add_action(
            &ui,
            &actions,
            "Keycode 30 release",
            REMOTE_DEVICE_KEYBOARD,
            Action::KeyboardKeycode {
                keycode: 30,
                state: 0,
            },
        );
        ui.set_controls();

        {
            let weak_ui = Rc::downgrade(&ui);
            ui.start.connect_clicked(move |_| {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.start();
                }
            });
        }
        {
            let weak_ui = Rc::downgrade(&ui);
            ui.stop.connect_clicked(move |_| {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.stop();
                }
            });
        }

        let shutdown_ui = Rc::clone(&ui);
        let shutdown: Rc<dyn Fn()> = Rc::new(move || {
            if shutdown_ui.closed.replace(true) {
                return;
            }
            shutdown_ui.stop();
        });

        ComputerControlPage { root, shutdown }
    }
}

#[cfg(not(desktop_controller_only))]
pub use native::{ComputerControlPage, build};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer_control::Action;

    #[test]
    fn cancellation_blocks_queued_input_dispatch() {
        assert!(input_dispatch_allowed(false));
        assert!(!input_dispatch_allowed(true));
    }

    #[test]
    fn screenshot_is_not_mapped_to_a_worker_input_call() {
        assert_eq!(
            map_approved_action(&Action::Screenshot { max_bytes: 1024 }),
            Err(MappingError::ScreenshotUnsupported)
        );
    }

    #[test]
    fn discrete_scroll_rejects_both_axes() {
        assert_eq!(
            map_approved_action(&Action::DiscreteScroll { dx: 1, dy: -1 }),
            Err(MappingError::DiscreteScrollBothAxes)
        );
    }

    #[test]
    fn approved_actions_map_to_typed_worker_calls() {
        assert_eq!(
            map_approved_action(&Action::KeyboardKeycode {
                keycode: 30,
                state: 1,
            }),
            Ok(MappedAction::KeyboardKeycode {
                keycode: 30,
                state: 1,
            })
        );
    }
}
