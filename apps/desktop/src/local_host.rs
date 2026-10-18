//! Session-scoped, explicitly enabled host for the selected personal account.
use muse_account::BridgeConfig;
use muse_local_link::HostEvent;
use muse_local_link::{HostConfig, HostSession};
use muse_local_tools::{ApprovalRequest, Decision, Invocation, ToolBroker, ToolReply};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{atomic::AtomicUsize, mpsc},
    time::{Duration, Instant},
};

struct Card {
    request: ApprovalRequest,
    root: PathBuf,
    grant_id: String,
    deadline: Instant,
    deciding: bool,
}
enum Completed {
    Grant(Result<(), &'static str>),
    Operation(Decision, PathBuf, String, Instant),
}
struct Completion {
    epoch: u64,
    value: Completed,
}
const MAX_WORK: usize = 8;
struct WorkGuard(Arc<AtomicUsize>);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(crate) struct DeviceLease {
    pub node_id: String,
    pub active: Arc<AtomicBool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Disconnected,
    Connecting,
    Registered,
}
struct Controller {
    epoch: u64,
    phase: Phase,
    broker: ToolBroker,
    session: Option<HostSession>,
    identity: Option<DeviceLease>,
    cards: Vec<Card>,
    replies: VecDeque<ToolReply>,
    workers: Arc<AtomicUsize>,
    tx: mpsc::SyncSender<Completion>,
    rx: mpsc::Receiver<Completion>,
    notice: String,
}
impl Controller {
    fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel(16);
        Self {
            epoch: 0,
            phase: Phase::Disconnected,
            broker: ToolBroker::new(),
            session: None,
            identity: None,
            cards: Vec::new(),
            replies: VecDeque::new(),
            workers: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
            notice: "Disconnected. No folders shared with Muse.".into(),
        }
    }
    fn device(&self) -> Option<DeviceLease> {
        self.identity
            .as_ref()
            .filter(|_| self.phase == Phase::Registered)
            .cloned()
    }
    fn start(&mut self, config: Option<BridgeConfig>, consent: bool) -> Result<(), &'static str> {
        if !consent
            || self.phase != Phase::Disconnected
            || self.workers.load(Ordering::SeqCst) != 0
            || self.broker.grants().is_empty()
        {
            return Err("Connect the account, consent and grant a folder before starting.");
        }
        let config = config.ok_or("Connect your Muse account first.")?;
        let mut nonce = [0u8; 16];
        // SAFETY: the writable buffer is valid for its supplied length.
        if unsafe { libc::getrandom(nonce.as_mut_ptr().cast(), nonce.len(), 0) }
            != nonce.len() as isize
        {
            return Err("Could not generate a secure session identity.");
        }
        let node_id = format!(
            "linux-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let script = config.script.with_file_name("muse_local_host.py");
        let session = HostSession::spawn(HostConfig {
            python: config.python,
            script,
            config_dir: config.config_dir,
            node_id: node_id.clone(),
            display_name: "Linux (session-only)".into(),
            commands: self.broker.manifest()["commands_v2"].clone(),
        })
        .map_err(|_| "Host configuration rejected.")?;
        self.epoch = self.epoch.wrapping_add(1);
        self.identity = Some(DeviceLease {
            node_id,
            active: Arc::new(AtomicBool::new(false)),
        });
        self.session = Some(session);
        self.phase = Phase::Connecting;
        self.notice = "Waiting for the actual registration acknowledgement; not paired yet.".into();
        Ok(())
    }
    fn work(&self, work: impl FnOnce() -> Completed + Send + 'static) -> Result<(), &'static str> {
        self.workers
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_WORK).then_some(n + 1)
            })
            .map_err(|_| "Local host is busy; no additional work was queued.")?;
        let guard = WorkGuard(Arc::clone(&self.workers));
        let tx = self.tx.clone();
        let epoch = self.epoch;
        std::thread::spawn(move || {
            let value = work();
            let _ = tx.try_send(Completion { epoch, value });
            drop(guard);
        });
        Ok(())
    }
    fn grant(&self, path: PathBuf, consent: bool) -> Result<(), &'static str> {
        if !consent || self.phase != Phase::Disconnected || self.workers.load(Ordering::SeqCst) != 0
        {
            return Err("Stop the host and explicitly consent before granting a folder.");
        }
        if self.broker.grants().len() >= 8 {
            return Err("At most eight folders may be granted.");
        }
        let broker = self.broker.clone();
        self.work(move || {
            Completed::Grant(
                broker
                    .grant(&path)
                    .map(|_| ())
                    .map_err(|_| "Folder access was denied."),
            )
        })
    }
    fn invoke(&mut self, invocation: muse_local_link::HostInvocation) {
        if self.phase != Phase::Registered {
            self.stop();
            return;
        }
        let grant_id = invocation
            .params
            .get("grant_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let root = self
            .broker
            .grants()
            .into_iter()
            .find(|g| g.id == grant_id)
            .map(|g| g.root)
            .unwrap_or_default();
        let deadline = Instant::now() + Duration::from_millis(invocation.timeout_ms.min(120000));
        let broker = self.broker.clone();
        if self
            .work(move || {
                let decision = run_invocation(&broker, invocation, deadline);
                Completed::Operation(decision, root, grant_id, deadline)
            })
            .is_err()
        {
            self.stop();
            self.notice = "Host stopped: local worker capacity reached.".into();
        }
    }
    fn decide(&mut self, id: &str, allow: bool) -> Result<(), &'static str> {
        self.poll(); // Closure/expiry wins over a click on an old rendered card.
        if self.phase != Phase::Registered {
            return Err("Host is disconnected.");
        }
        let index = self
            .cards
            .iter()
            .position(|c| c.request.approval_id == id && !c.deciding && Instant::now() < c.deadline)
            .ok_or("Approval expired or revoked.")?;
        let broker = self.broker.clone();
        let id = id.to_owned();
        self.work(move || {
            Completed::Operation(
                Decision::Reply(if allow {
                    broker.approve(&id)
                } else {
                    broker.deny(&id)
                }),
                PathBuf::new(),
                String::new(),
                Instant::now(),
            )
        })?;
        self.cards[index].deciding = true;
        Ok(())
    }
    fn poll(&mut self) {
        // Bounded drains keep GTK responsive even under a hostile event stream.
        for _ in 0..16 {
            let event = self.session.as_ref().map(|s| s.try_recv());
            match event {
                Some(Ok(HostEvent::Registered)) => self.registered(self.epoch),
                Some(Ok(HostEvent::Connecting)) => {}
                Some(Ok(HostEvent::Invoke(invocation))) => self.invoke(invocation),
                Some(Ok(HostEvent::Closed { .. }))
                | Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.stop();
                    self.notice="Host connection closed. Review your account, then grant folders and start again.".into();
                    break;
                }
                _ => break,
            }
        }
        for _ in 0..16 {
            let Ok(completed) = self.rx.try_recv() else {
                break;
            };
            if completed.epoch != self.epoch {
                continue;
            }
            match completed.value {
                Completed::Grant(result) => {
                    self.notice = match result {
                        Ok(()) => "Folder granted for this local-host session only.".into(),
                        Err(e) => e.into(),
                    }
                }
                Completed::Operation(decision, root, grant_id, deadline)
                    if self.phase == Phase::Registered =>
                {
                    match decision {
                        Decision::Reply(reply) => {
                            self.cards.retain(|c| c.request.invocation_id != reply.id);
                            self.replies.push_back(reply);
                        }
                        Decision::Approval(request) => {
                            if Instant::now() < deadline {
                                self.cards.push(Card {
                                    request,
                                    root,
                                    grant_id,
                                    deadline,
                                    deciding: false,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if self.phase == Phase::Registered {
            for reply in self.broker.expire() {
                self.cards.retain(|c| c.request.invocation_id != reply.id);
                self.replies.push_back(reply);
            }
            if self.replies.len() > 16 {
                self.stop();
                return;
            }
            for _ in 0..16 {
                let Some(reply) = self.replies.front() else {
                    break;
                };
                let Some(session) = self.session.as_ref() else {
                    break;
                };
                match session.send_result(
                    &reply.id,
                    reply.ok,
                    reply.output.clone(),
                    reply.error.clone(),
                ) {
                    Ok(()) => {
                        self.replies.pop_front();
                    }
                    Err(e) if matches!(e.code, "busy" | "queue_full") => break,
                    Err(e) if e.code == "request_too_large" => {
                        let reply = self.replies.front_mut().unwrap();
                        reply.ok = false;
                        reply.output = serde_json::Value::Null;
                        reply.error = Some("result_too_large".into());
                    }
                    Err(_) => {
                        self.replies.pop_front();
                    }
                }
            }
        }
    }
    fn registered(&mut self, epoch: u64) {
        if epoch == self.epoch && self.phase == Phase::Connecting {
            self.phase = Phase::Registered;
            self.notice =
                "Registered for this session. Stop revokes folders and pending approvals.".into();
            if let Some(identity) = &self.identity {
                identity.active.store(true, Ordering::SeqCst);
            }
        }
    }
    fn stop(&mut self) {
        // Cancellation boundary: stop transport BEFORE revoking file capabilities.
        if let Some(session) = self.session.take() {
            session.stop();
        }
        if let Some(identity) = self.identity.take() {
            identity.active.store(false, Ordering::SeqCst);
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.phase = Phase::Disconnected;
        self.cards.clear();
        self.replies.clear();
        self.notice =
            "Disconnected. Grants revoked; new consent, folders and Start are required.".into();
        self.broker.revoke_all();
        self.broker = ToolBroker::new();
    }
}
fn run_invocation(
    broker: &ToolBroker,
    invocation: muse_local_link::HostInvocation,
    deadline: Instant,
) -> Decision {
    // Scheduling must consume, never restart, the remote invocation's budget.
    let timeout_ms = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(120000) as u64;
    broker.invoke(Invocation {
        id: invocation.id,
        command: invocation.command,
        params: invocation.params,
        timeout_ms,
    })
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) use native::build;
pub(crate) type SharedDevice = std::rc::Rc<std::cell::RefCell<Option<DeviceLease>>>;
pub(crate) type AccountContext = std::rc::Rc<dyn Fn() -> Option<BridgeConfig>>;
pub(crate) type BoundaryHook = std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>>>;

mod native {
    use super::*;
    use gtk::prelude::*;
    use gtk4 as gtk;
    use std::{cell::RefCell, rc::Rc};
    pub(crate) struct HostPage {
        pub root: gtk::Box,
        pub shutdown: Rc<dyn Fn()>,
        #[cfg(test)]
        pub(super) ui: Rc<Ui>,
    }
    pub(super) struct CardWidgets {
        id: String,
        pub(super) content: gtk::TextView,
        countdown: gtk::Label,
        pub(super) allow: gtk::Button,
        deny: gtk::Button,
    }
    pub(super) struct Ui {
        pub(super) host: RefCell<Controller>,
        context: AccountContext,
        device: SharedDevice,
        root: gtk::Box,
        pub(super) consent: gtk::CheckButton,
        pub(super) choose: gtk::Button,
        pub(super) start: gtk::Button,
        stop: gtk::Button,
        status: gtk::Label,
        grants: gtk::Label,
        cards: gtk::Box,
        pub(super) dialog: crate::DialogLease<gtk::FileChooserNative>,
        pub(super) widgets: RefCell<Vec<CardWidgets>>,
    }
    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }
    pub(crate) fn build(context: AccountContext, device: SharedDevice) -> HostPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label("Local host · your existing Muse agent on Linux"));
        root.append(&label("Separate from account connection and the read-only Local files browser. When enabled, Muse may search filenames and read text inside the folders you grant. Filenames and content reach Muse. Each CREATE NEW text-file operation requires your local Allow once approval of its exact destination and full content. No overwrite, delete, shell or computer control."));
        root.append(&label("Identity is cryptographically random and session-only. Chats are attributed to this Linux host only after registration. Stop, account disconnect/error/change, or window close revokes all grants and approvals. No automatic restart or persistent broad grants; existing Muse settings are never changed."));
        let consent = gtk::CheckButton::with_label(
            "I explicitly enable this local host and allow granted filenames and text to reach Muse.",
        );
        root.append(&consent);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        toolbar.add_css_class("toolbar");
        let choose = gtk::Button::with_label("Grant a folder to Muse…");
        let start = gtk::Button::with_label("Start local host");
        start.add_css_class("suggested-action");
        let stop = gtk::Button::with_label("Stop & revoke all");
        stop.add_css_class("destructive-action");
        for b in [&choose, &start, &stop] {
            toolbar.append(b);
        }
        root.append(&toolbar);
        let status = label("");
        status.add_css_class("chat-status");
        root.append(&status);
        let grants = label("No folders granted.");
        grants.add_css_class("dim-label");
        root.append(&grants);
        let approvals_heading =
            label("Pending create approvals · review the entire immutable content before allowing");
        approvals_heading.add_css_class("section-heading");
        root.append(&approvals_heading);
        let cards = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .min_content_height(180)
            .build();
        scroll.set_child(Some(&cards));
        root.append(&scroll);
        let ui = Rc::new(Ui {
            host: RefCell::new(Controller::new()),
            context,
            device,
            root: root.clone(),
            consent,
            choose,
            start,
            stop,
            status,
            grants,
            cards,
            dialog: crate::DialogLease::default(),
            widgets: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&ui);
        ui.consent.connect_toggled(move |_| {
            if let Some(u) = weak.upgrade() {
                u.render();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.choose.connect_clicked(move |_| {
            if let Some(u) = weak.upgrade() {
                u.choose_folder();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.start.connect_clicked(move |_| {
            if let Some(u) = weak.upgrade() {
                if u.dialog.is_active() {
                    return;
                }
                let result = u
                    .host
                    .borrow_mut()
                    .start((u.context)(), u.consent.is_active());
                if let Err(e) = result {
                    u.host.borrow_mut().notice = e.into();
                }
                u.render();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.stop.connect_clicked(move |_| {
            if let Some(u) = weak.upgrade() {
                u.shutdown();
            }
        });
        let weak = Rc::downgrade(&ui);
        gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(u) = weak.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            if u.host.borrow().phase != Phase::Disconnected && (u.context)().is_none() {
                u.shutdown();
            }
            let before = u.host.borrow().phase;
            u.host.borrow_mut().poll();
            if before != Phase::Disconnected && u.host.borrow().phase == Phase::Disconnected {
                u.consent.set_active(false);
            }
            u.render();
            gtk::glib::ControlFlow::Continue
        });
        ui.render();
        let owner = Rc::clone(&ui);
        HostPage {
            root,
            shutdown: Rc::new(move || owner.shutdown()),
            #[cfg(test)]
            ui,
        }
    }
    impl Ui {
        pub(super) fn shutdown(self: &Rc<Self>) {
            self.host.borrow_mut().stop();
            self.device.borrow_mut().take();
            if let Some(dialog) = self.dialog.active.borrow_mut().take() {
                dialog.hide();
            }
            self.consent.set_active(false);
            self.render();
        }
        fn choose_folder(self: &Rc<Self>) {
            if self.dialog.is_active()
                || !self.consent.is_active()
                || (self.context)().is_none()
                || self.host.borrow().phase != Phase::Disconnected
            {
                return;
            }
            let parent = self
                .root
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok());
            let dialog = gtk::FileChooserNative::new(
                Some("Grant folder filenames and text to Muse"),
                parent.as_ref(),
                gtk::FileChooserAction::SelectFolder,
                Some("Grant to Muse"),
                Some("Cancel"),
            );
            let weak = Rc::downgrade(self);
            let epoch = self.host.borrow().epoch;
            dialog.connect_response(move |chooser, response| {
                if let Some(u) = weak.upgrade() {
                    if response == gtk::ResponseType::Accept
                        && u.host.borrow().epoch == epoch
                        && u.consent.is_active()
                        && (u.context)().is_some()
                        && let Some(path) = chooser.file().and_then(|f| f.path())
                    {
                        let result = u.host.borrow().grant(path, true);
                        if let Err(e) = result {
                            u.host.borrow_mut().notice = e.into();
                        }
                    }
                    chooser.hide();
                    u.dialog.active.borrow_mut().take();
                    u.render();
                } else {
                    chooser.hide();
                }
            });
            self.dialog.keep(dialog.clone());
            dialog.show();
            self.render();
        }
        fn render(self: &Rc<Self>) {
            let host = self.host.borrow();
            *self.device.borrow_mut() = host.device();
            let idle = host.phase == Phase::Disconnected;
            let working = host.workers.load(Ordering::SeqCst) > 0;
            let connected = (self.context)().is_some();
            self.consent.set_sensitive(idle);
            self.choose.set_sensitive(
                idle && !working
                    && connected
                    && self.consent.is_active()
                    && !self.dialog.is_active(),
            );
            self.start.set_sensitive(
                idle && !working
                    && connected
                    && self.consent.is_active()
                    && !host.broker.grants().is_empty()
                    && !self.dialog.is_active(),
            );
            self.stop.set_sensitive(
                !idle || !host.broker.grants().is_empty() || working || self.dialog.is_active(),
            );
            self.status.set_text(&format!(
                "{} · {}",
                match host.phase {
                    Phase::Disconnected => "Disconnected",
                    Phase::Connecting => "Connecting (not registered)",
                    Phase::Registered => "Registered with Muse",
                },
                host.notice
            ));
            let grants = host.broker.grants();
            self.grants.set_text(&if grants.is_empty() {
                "No folders granted.".into()
            } else {
                grants
                    .iter()
                    .map(|g| format!("{} · {}\n{}", g.label, g.id, g.root.display()))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            });
            let changed = {
                let widgets = self.widgets.borrow();
                widgets.len() != host.cards.len()
                    || widgets
                        .iter()
                        .zip(&host.cards)
                        .any(|(w, c)| w.id != c.request.approval_id)
            };
            if changed {
                crate::clear_chat(&self.cards);
                self.widgets.borrow_mut().clear();
                for card in &host.cards {
                    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
                    body.add_css_class("card");
                    body.add_css_class("approval-card");
                    // Title row: icon + bold question. The heading text itself is
                    // unchanged; only presentation moves around it.
                    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let icon = gtk::Label::new(Some("⚠"));
                    icon.add_css_class("approval-icon");
                    icon.set_valign(gtk::Align::Start);
                    title_row.append(&icon);
                    let heading = label(&format!(
                        "CREATE NEW text file (no overwrite)\nGrant: {}\nFolder: {}\nRelative destination: {}\nFull destination: {}",
                        card.grant_id,
                        card.root.display(),
                        card.request.path,
                        card.root.join(&card.request.path).display()
                    ));
                    heading.add_css_class("approval-heading");
                    heading.set_hexpand(true);
                    title_row.append(&heading);
                    body.append(&title_row);
                    let content = gtk::TextView::new();
                    content.set_editable(false);
                    content.set_cursor_visible(false);
                    content.set_monospace(true);
                    content.set_wrap_mode(gtk::WrapMode::WordChar);
                    content.buffer().set_text(&card.request.content);
                    let scroll = gtk::ScrolledWindow::builder()
                        .hexpand(true)
                        .min_content_height(180)
                        .build();
                    scroll.add_css_class("approval-scroll");
                    scroll.set_child(Some(&content));
                    // Expandable detail box (Mac pattern); expanded by default so
                    // the entire immutable content stays reviewable. Read-only.
                    let details = gtk::Expander::new(Some(&format!(
                        "Full immutable content · {} characters",
                        card.request.content.chars().count()
                    )));
                    details.add_css_class("approval-details");
                    details.set_expanded(true);
                    details.set_child(Some(&scroll));
                    body.append(&details);
                    let countdown = label("");
                    countdown.add_css_class("approval-countdown");
                    body.append(&countdown);
                    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    actions.set_halign(gtk::Align::End);
                    actions.add_css_class("approval-actions");
                    let allow = gtk::Button::with_label("Allow once");
                    allow.add_css_class("suggested-action");
                    let deny = gtk::Button::with_label("Deny");
                    actions.append(&allow);
                    actions.append(&deny);
                    body.append(&actions);
                    self.cards.append(&body);
                    for (button, allowing) in [(&allow, true), (&deny, false)] {
                        let weak = Rc::downgrade(self);
                        let id = card.request.approval_id.clone();
                        button.connect_clicked(move |_| {
                            if let Some(u) = weak.upgrade() {
                                let result = u.host.borrow_mut().decide(&id, allowing);
                                if let Err(e) = result {
                                    u.host.borrow_mut().notice = e.into();
                                }
                                u.render();
                            }
                        });
                    }
                    self.widgets.borrow_mut().push(CardWidgets {
                        id: card.request.approval_id.clone(),
                        content,
                        countdown,
                        allow,
                        deny,
                    });
                }
            }
            for (widgets, card) in self.widgets.borrow().iter().zip(&host.cards) {
                let remaining = card.deadline.saturating_duration_since(Instant::now());
                let enabled =
                    host.phase == Phase::Registered && !card.deciding && !remaining.is_zero();
                widgets.allow.set_sensitive(enabled);
                widgets.deny.set_sensitive(enabled);
                widgets.countdown.set_text(&if card.deciding {
                    "Decision pending…".into()
                } else {
                    format!(
                        "Expires in {} seconds (monotonic deadline)",
                        remaining
                            .as_secs()
                            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
                    )
                });
                // Read-only even if a theme or focus action changes widget state.
                widgets.content.set_editable(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant},
    };
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(command: &str, content: &str, timeout: u64) -> Self {
            let path = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .expect("scratch TMPDIR")
                .join(format!(
                    "host-ui-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::SeqCst)
                ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("muse_local_host.py"), format!(r#"
import json,sys,pathlib,time
r=json.loads(sys.stdin.readline())
pathlib.Path(__file__).with_name('identity').write_text(r['node_id'])
print(json.dumps({{'type':'state','state':'connecting'}}),flush=True)
time.sleep(.05)
print(json.dumps({{'type':'state','state':'registered'}}),flush=True)
grant=r['commands_v2']['files.create_text']['required']['grant_id']['enum'][0]
print(json.dumps({{'type':'invoke','id':'one','command':{command:?},'params':{{'grant_id':grant,'path':'new.txt','content':{content:?}}},'timeout_ms':{timeout}}}),flush=True)
for line in sys.stdin:
    r=json.loads(line)
    if r['op']=='stop': break
    pathlib.Path(__file__).with_name('reply').write_text(json.dumps(r))
"#)).unwrap();
            Self(path)
        }
        fn config(&self) -> BridgeConfig {
            BridgeConfig {
                python: "/usr/bin/python3".into(),
                script: self.0.join("muse_account_bridge.py"),
                config_dir: self.0.clone(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn pump(host: &mut Controller, predicate: impl Fn(&Controller) -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !predicate(host) {
            assert!(Instant::now() < until, "host condition timed out");
            host.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn real_transport_broker_approval_creates_only_after_allow_and_delivers_result() {
        let f = Fixture::new("files.create_text", "exact full content\n", 30000);
        let mut host = Controller::new();
        host.grant(f.0.clone(), true).unwrap();
        pump(&mut host, |h| !h.broker.grants().is_empty());
        host.start(Some(f.config()), true).unwrap();
        assert_eq!(host.phase, Phase::Connecting);
        assert!(host.device().is_none());
        pump(&mut host, |h| !h.cards.is_empty());
        let device = host.device().unwrap();
        assert_eq!(
            device.node_id,
            fs::read_to_string(f.0.join("identity")).unwrap()
        );
        let card = &host.cards[0];
        assert_eq!(card.root, f.0);
        assert_eq!(card.request.content, "exact full content\n");
        assert!(!f.0.join("new.txt").exists());
        let id = card.request.approval_id.clone();
        host.decide(&id, true).unwrap();
        pump(&mut host, |_| f.0.join("reply").exists());
        assert_eq!(
            fs::read_to_string(f.0.join("new.txt")).unwrap(),
            "exact full content\n"
        );
        let reply: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(f.0.join("reply")).unwrap()).unwrap();
        assert_eq!(reply["id"], "one");
        assert_eq!(reply["ok"], true);
        assert!(host.grant(f.0.clone(), true).is_err());
        host.stop();
        assert!(!device.active.load(Ordering::SeqCst));
        assert!(host.cards.is_empty());
        assert!(host.broker.grants().is_empty());
        assert!(host.start(Some(f.config()), true).is_err());
    }
    #[test]
    #[ignore = "requires GTK display; run under xvfb-run --test-threads=1"]
    fn native_host_card_approval_and_account_boundary_smoke() {
        use gtk::glib::translate::IntoGlib;
        use gtk::prelude::*;
        use gtk4 as gtk;
        use std::{cell::RefCell, rc::Rc};
        gtk::init().unwrap();
        let f = Fixture::new("files.create_text", "native exact content\n", 30000);
        let config = f.config();
        let device = Rc::new(RefCell::new(None));
        let page = native::build(Rc::new(move || Some(config.clone())), device.clone());
        let window = gtk::Window::new();
        window.set_child(Some(&page.root));
        window.present();
        assert!(!page.ui.choose.is_sensitive());
        assert!(!page.ui.start.is_sensitive());
        page.ui.consent.set_active(true);
        page.ui.choose.emit_clicked();
        assert!(
            page.ui.dialog.is_active(),
            "native chooser must retain its owned lease"
        );
        let chooser = page.ui.dialog.active.borrow().as_ref().unwrap().clone();
        chooser.set_file(&gtk::gio::File::for_path(&f.0)).unwrap();
        let context = gtk::glib::MainContext::default();
        let ready = Instant::now() + Duration::from_millis(200);
        while Instant::now() < ready {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        chooser.emit_by_name::<()>("response", &[&gtk::ResponseType::Accept.into_glib()]);
        assert!(!page.ui.dialog.is_active());
        let until = Instant::now() + Duration::from_secs(5);
        while !page.ui.start.is_sensitive() {
            assert!(Instant::now() < until);
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        page.ui.start.emit_clicked();
        assert!(device.borrow().is_none());
        while page.ui.widgets.borrow().is_empty() {
            assert!(Instant::now() < until);
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(device.borrow().is_some());
        assert!(!f.0.join("new.txt").exists());
        let widgets = page.ui.widgets.borrow();
        assert!(!widgets[0].content.is_editable());
        assert_eq!(
            widgets[0]
                .content
                .buffer()
                .text(
                    &widgets[0].content.buffer().start_iter(),
                    &widgets[0].content.buffer().end_iter(),
                    true
                )
                .as_str(),
            "native exact content\n"
        );
        let allow = widgets[0].allow.clone();
        // Mac-pattern card structure: read-only content lives inside a scroll
        // inside an expanded details box; actions are exactly Allow once
        // (filled) and Deny — never an Always-allow.
        let widgets_ref = page.ui.widgets.borrow();
        let content = widgets_ref[0].content.clone();
        assert!(widgets_ref[0].allow.has_css_class("suggested-action"));
        drop(widgets_ref);
        let mut in_scroll = false;
        let mut in_details = false;
        let mut node = content.parent();
        while let Some(widget) = node {
            if widget.is::<gtk::ScrolledWindow>() {
                in_scroll = true;
            }
            if widget.is::<gtk::Expander>() {
                in_details = true;
            }
            node = widget.parent();
        }
        assert!(in_scroll, "approval content must sit in a read-only scroll");
        assert!(in_details, "approval content must sit in a details box");
        let mut labels = Vec::new();
        let mut stack = vec![page.root.clone().upcast::<gtk::Widget>()];
        while let Some(widget) = stack.pop() {
            if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
                labels.push(button.label().unwrap_or_default().to_string());
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                stack.push(next.clone());
                child = next.next_sibling();
            }
        }
        assert!(labels.iter().any(|label| label == "Allow once"));
        assert!(labels.iter().any(|label| label == "Deny"));
        assert!(
            !labels.iter().any(|label| label.contains("Always")),
            "one-time approvals only: no Always-allow button"
        );
        drop(widgets);
        allow.emit_clicked();
        while !f.0.join("new.txt").exists() {
            assert!(Instant::now() < until);
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        let lease = device.borrow().clone().unwrap();
        (page.shutdown)();
        assert!(device.borrow().is_none());
        assert!(!lease.active.load(Ordering::SeqCst));
        assert!(page.ui.widgets.borrow().is_empty());
        assert!(!page.ui.consent.is_active());
        window.close();
    }
    #[test]
    fn worker_budget_cannot_extend_remote_deadline() {
        let broker = ToolBroker::new();
        let f = Fixture::new("files.create_text", "never", 30000);
        let grant = broker.grant(&f.0).unwrap();
        let invocation = muse_local_link::HostInvocation {
            id: "late".into(),
            command: "files.create_text".into(),
            params: serde_json::json!({"grant_id":grant.id,"path":"late.txt","content":"never"}),
            timeout_ms: 30000,
        };
        let deadline = Instant::now() - Duration::from_millis(1);
        let decision = run_invocation(&broker, invocation, deadline);
        assert!(matches!(
            decision,
            Decision::Reply(ToolReply { ok: false, .. })
        ));
        assert!(!f.0.join("late.txt").exists());
    }
    #[test]
    fn deny_expiry_revoke_and_stale_completions_never_mutate() {
        for outcome in ["deny", "expiry", "stop"] {
            let f = Fixture::new(
                "files.create_text",
                "never",
                if outcome == "expiry" { 500 } else { 30000 },
            );
            let mut host = Controller::new();
            host.grant(f.0.clone(), true).unwrap();
            pump(&mut host, |h| !h.broker.grants().is_empty());
            host.start(Some(f.config()), true).unwrap();
            pump(&mut host, |h| !h.cards.is_empty());
            let id = host.cards[0].request.approval_id.clone();
            let epoch = host.epoch;
            match outcome {
                "deny" => {
                    host.decide(&id, false).unwrap();
                    pump(&mut host, |h| h.cards.is_empty());
                }
                "expiry" => {
                    std::thread::sleep(Duration::from_millis(550));
                    assert!(host.decide(&id, true).is_err());
                    host.poll();
                }
                _ => host.stop(),
            }
            assert!(!f.0.join("new.txt").exists());
            assert!(host.decide(&id, true).is_err());
            host.stop();
            let stale = ApprovalRequest {
                approval_id: "stale".into(),
                invocation_id: "one".into(),
                title: "stale".into(),
                path: "new.txt".into(),
                content: "never".into(),
                expires_in_seconds: 60,
            };
            host.tx
                .try_send(Completion {
                    epoch,
                    value: Completed::Operation(
                        Decision::Approval(stale),
                        f.0.clone(),
                        "old".into(),
                        Instant::now() + Duration::from_secs(60),
                    ),
                })
                .unwrap();
            host.poll();
            assert!(host.cards.is_empty());
            assert!(host.replies.is_empty());
        }
    }
    #[test]
    fn consent_registration_and_stop_define_device_lifetime() {
        let mut host = Controller::new();
        assert!(host.start(None, false).is_err());
        assert!(host.device().is_none());
        assert_eq!(host.phase, Phase::Disconnected);
        let old = host.epoch;
        host.stop();
        host.registered(old);
        assert_eq!(host.phase, Phase::Disconnected);
        assert!(host.device().is_none());
        assert!(host.broker.grants().is_empty());
    }
}
