//! Local reminders UI over the durable `muse-tasks` store.
//!
//! One-shot delayed reminders only: no command execution, no background
//! polling, no notification daemon. Every store op runs inline on an
//! explicit button press against a private app directory (0700); due
//! reminders surface only when the user presses Check due, and
//! acknowledging them is a separate explicit press.
#![allow(unexpected_cfgs)]

#[cfg(not(tasks_controller_only))]
mod native {
    use gtk::prelude::*;
    use gtk4 as gtk;
    use muse_tasks::{Task, TaskError, TaskStore};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    pub struct TasksPage {
        pub root: gtk::Box,
        #[cfg(test)]
        #[allow(dead_code)]
        pub(super) ui: Rc<Ui>,
    }

    pub(super) struct Ui {
        dir: PathBuf,
        tasks: RefCell<Vec<Task>>,
        shown_due: RefCell<Vec<u64>>,
        list: gtk::ListBox,
        list_notice: gtk::Label,
        title: gtk::Entry,
        body: gtk::Entry,
        due_in: gtk::Entry,
        create_notice: gtk::Label,
        due_notice: gtk::Label,
    }

    fn label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        label
    }

    fn default_dir() -> Option<PathBuf> {
        // Never fall back to a shared fixed path: an unset/empty HOME must
        // refuse, not land storage in a predictable world-writable location.
        let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".config/unofficial-muse-for-linux/tasks"))
    }

    /// Fail-closed stand-in used only when HOME is unset; nothing can be
    /// created beneath it, so every store op reports unavailability.
    fn no_home_dir() -> PathBuf {
        PathBuf::from("/proc/self/muse-tasks-home-unset")
    }

    /// Parse a relative due offset like `30m`, `2h`, `3d` into seconds.
    /// Splits on the last char (never a byte index) so non-ASCII input
    /// fails closed instead of panicking on a char boundary.
    pub(super) fn parse_due_in(text: &str) -> Option<u64> {
        let text = text.trim();
        let mut chars = text.chars();
        let unit = chars.next_back()?;
        if !matches!(unit, 'm' | 'h' | 'd') {
            return None;
        }
        let digits = &text[..text.len() - unit.len_utf8()];
        let n: u64 = digits.parse().ok()?;
        if n == 0 {
            return None;
        }
        let secs = match unit {
            'm' => n.checked_mul(60)?,
            'h' => n.checked_mul(3600)?,
            'd' => n.checked_mul(86400)?,
            _ => return None,
        };
        if secs > muse_tasks::FUTURE_WINDOW_SECS {
            return None;
        }
        Some(secs)
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Human relative time: `in 3h 12m` / `25m overdue` / `due now`.
    pub(super) fn relative_due(due: u64, now: u64) -> String {
        if due > now {
            let mut left = due - now;
            let days = left / 86400;
            left %= 86400;
            let hours = left / 3600;
            let mins = (left % 3600) / 60;
            if days > 0 {
                format!("in {days}d {hours}h")
            } else if hours > 0 {
                format!("in {hours}h {mins}m")
            } else if mins > 0 {
                format!("in {mins}m")
            } else {
                "due now".to_string()
            }
        } else if now - due < 60 {
            "due now".to_string()
        } else {
            let over = now - due;
            if over >= 86400 {
                format!("{}d overdue", over / 86400)
            } else if over >= 3600 {
                format!("{}h overdue", over / 3600)
            } else {
                format!("{}m overdue", over / 60)
            }
        }
    }

    impl Ui {
        fn open_store(&self) -> Result<TaskStore, String> {
            TaskStore::open(&self.dir).map_err(|e| e.to_string())
        }

        fn refresh(&self) {
            match self.open_store() {
                Ok(store) => {
                    let tasks = store.list();
                    self.tasks.replace(tasks);
                    self.render_list();
                    self.list_notice.set_text(
                        "Reminders are local only. Nothing polls; press Check due to see what is due.",
                    );
                }
                Err(error) => {
                    self.list_notice
                        .set_text(&format!("Store unavailable: {error} Nothing was loaded."));
                }
            }
        }

        fn render_list(&self) {
            while let Some(row) = self.list.first_child() {
                self.list.remove(&row);
            }
            let now = now_secs();
            for task in self.tasks.borrow().iter() {
                let status = match task.status {
                    muse_tasks::TaskStatus::Pending => "pending",
                    muse_tasks::TaskStatus::Paused => "paused",
                    muse_tasks::TaskStatus::Done => "done",
                    muse_tasks::TaskStatus::Cancelled => "cancelled",
                };
                let row = label(&format!(
                    "#{} [{}] {} — {}{}",
                    task.id,
                    status,
                    task.title,
                    relative_due(task.due_unix_secs, now),
                    if task.notified { " (notified)" } else { "" },
                ));
                self.list.append(&row);
            }
        }

        fn selected_id(&self) -> Option<u64> {
            let row = self.list.selected_row()?;
            let idx = row.index() as usize;
            self.tasks.borrow().get(idx).map(|t| t.id)
        }

        fn mutate(
            &self,
            what: &str,
            op: impl FnOnce(&mut TaskStore, u64) -> Result<Task, TaskError>,
        ) {
            let Some(id) = self.selected_id() else {
                self.list_notice
                    .set_text("Select a reminder in the list first.");
                return;
            };
            match self.open_store() {
                Ok(mut store) => match op(&mut store, id) {
                    Ok(_) => {
                        self.refresh();
                        self.list_notice
                            .set_text(&format!("{what} applied to reminder #{id}."));
                    }
                    Err(error) => {
                        self.list_notice
                            .set_text(&format!("{what} refused: {error} Nothing changed."));
                    }
                },
                Err(error) => {
                    self.list_notice
                        .set_text(&format!("Store unavailable: {error} Nothing changed."));
                }
            }
        }

        fn create(&self) {
            let title = self.title.text().trim().to_string();
            let body = self.body.text().trim().to_string();
            let Some(offset) = parse_due_in(&self.due_in.text()) else {
                self.create_notice
                    .set_text("Due must be relative like 30m, 2h or 3d (up to 5 years).");
                return;
            };
            let due = now_secs().saturating_add(offset);
            match self.open_store() {
                Ok(mut store) => {
                    match store.create(&title, &body, due).map_err(|e| e.to_string()) {
                        Ok(task) => {
                            self.title.set_text("");
                            self.body.set_text("");
                            self.due_in.set_text("");
                            self.refresh();
                            self.create_notice.set_text(&format!(
                                "Reminder #{} created, {}.",
                                task.id,
                                relative_due(task.due_unix_secs, now_secs())
                            ));
                        }
                        Err(error) => {
                            self.create_notice
                                .set_text(&format!("Create refused: {error} Nothing was stored."));
                        }
                    }
                }
                Err(error) => {
                    self.create_notice
                        .set_text(&format!("Store unavailable: {error} Nothing was stored."));
                }
            }
        }

        fn check_due(&self) {
            match self.open_store() {
                Ok(store) => {
                    let due = store.due(now_secs());
                    if due.is_empty() {
                        self.due_notice.set_text("Nothing due right now.");
                        self.shown_due.replace(Vec::new());
                    } else {
                        let lines: Vec<String> = due
                            .iter()
                            .map(|t| format!("#{}: {}", t.id, t.title))
                            .collect();
                        self.due_notice.set_text(&format!(
                            "Due now (local reminders, nothing was sent anywhere):\n{}",
                            lines.join("\n")
                        ));
                        self.shown_due.replace(due.iter().map(|t| t.id).collect());
                    }
                    self.refresh();
                }
                Err(error) => {
                    self.due_notice
                        .set_text(&format!("Store unavailable: {error}"));
                }
            }
        }

        fn acknowledge(&self) {
            let ids = self.shown_due.borrow().clone();
            if ids.is_empty() {
                self.due_notice
                    .set_text("Nothing shown to acknowledge. Press Check due first.");
                return;
            }
            match self.open_store() {
                Ok(mut store) => {
                    let mut count = 0;
                    for id in &ids {
                        if store.mark_notified(*id).is_ok() {
                            count += 1;
                        }
                    }
                    self.shown_due.replace(Vec::new());
                    self.refresh();
                    self.due_notice.set_text(&format!(
                        "{count} reminder(s) acknowledged. They will not show as due again."
                    ));
                }
                Err(error) => {
                    self.due_notice
                        .set_text(&format!("Store unavailable: {error} Nothing changed."));
                }
            }
        }
    }

    pub fn build() -> TasksPage {
        match default_dir() {
            Some(dir) => build_with_dir(dir, None),
            None => build_with_dir(
                no_home_dir(),
                Some(
                    "HOME is not set; reminders are unavailable in this session. Nothing was stored.",
                ),
            ),
        }
    }

    pub fn build_with_dir(dir: PathBuf, preset_notice: Option<&str>) -> TasksPage {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("page-content");
        root.append(&label(
            "Reminders · durable local tasks, explicit checks only",
        ));
        root.append(&label(
            "One-shot local reminders. No command execution, no background polling, nothing leaves this machine. Due items appear only when you press Check due.",
        ));

        let create_heading = label("New reminder");
        create_heading.add_css_class("section-heading");
        root.append(&create_heading);
        let title = gtk::Entry::new();
        title.set_placeholder_text(Some("Title (1–200 chars)"));
        root.append(&title);
        let body = gtk::Entry::new();
        body.set_placeholder_text(Some("Details (optional, one line)"));
        root.append(&body);
        let due_in = gtk::Entry::new();
        due_in.set_placeholder_text(Some("Due in, e.g. 30m, 2h, 3d"));
        root.append(&due_in);
        let create = gtk::Button::with_label("Create reminder");
        create.add_css_class("suggested-action");
        root.append(&create);
        let create_notice = label("No reminder created yet.");
        if let Some(preset) = preset_notice {
            create_notice.set_text(preset);
        }
        root.append(&create_notice);

        let list_heading = label("Stored reminders");
        list_heading.add_css_class("section-heading");
        root.append(&list_heading);
        let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        action_row.add_css_class("toolbar");
        let refresh = gtk::Button::with_label("Refresh");
        let pause = gtk::Button::with_label("Pause selected");
        let resume = gtk::Button::with_label("Resume selected");
        let complete = gtk::Button::with_label("Complete selected");
        let cancel = gtk::Button::with_label("Cancel selected");
        for button in [&refresh, &pause, &resume, &complete, &cancel] {
            action_row.append(button);
        }
        root.append(&action_row);
        let list_notice = label("Press Refresh to load stored reminders.");
        root.append(&list_notice);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_scroll = gtk::ScrolledWindow::new();
        list_scroll.set_min_content_height(120);
        list_scroll.set_child(Some(&list));
        root.append(&list_scroll);

        let due_heading = label("Due check (explicit)");
        due_heading.add_css_class("section-heading");
        root.append(&due_heading);
        let due_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        due_row.add_css_class("toolbar");
        let check = gtk::Button::with_label("Check due now");
        let ack = gtk::Button::with_label("Acknowledge shown");
        due_row.append(&check);
        due_row.append(&ack);
        root.append(&due_row);
        let due_notice = label("No due check yet.");
        root.append(&due_notice);

        let ui = Rc::new(Ui {
            dir,
            tasks: RefCell::new(Vec::new()),
            shown_due: RefCell::new(Vec::new()),
            list,
            list_notice,
            title,
            body,
            due_in,
            create_notice,
            due_notice,
        });
        {
            let ui = Rc::clone(&ui);
            create.connect_clicked(move |_| ui.create());
        }
        {
            let ui = Rc::clone(&ui);
            refresh.connect_clicked(move |_| ui.refresh());
        }
        let bind = |button: &gtk::Button,
                    what: &'static str,
                    op: fn(&mut TaskStore, u64) -> Result<Task, TaskError>| {
            let ui = Rc::clone(&ui);
            button.connect_clicked(move |_| ui.mutate(what, op));
        };
        bind(&pause, "Pause", |s, id| s.pause(id));
        bind(&resume, "Resume", |s, id| s.resume(id));
        bind(&complete, "Complete", |s, id| s.complete(id));
        bind(&cancel, "Cancel", |s, id| s.cancel(id));
        {
            let ui = Rc::clone(&ui);
            check.connect_clicked(move |_| ui.check_due());
        }
        {
            let ui = Rc::clone(&ui);
            ack.connect_clicked(move |_| ui.acknowledge());
        }

        TasksPage {
            root,
            #[cfg(test)]
            ui,
        }
    }
}

#[cfg(not(tasks_controller_only))]
pub use native::{TasksPage, build};

#[cfg(test)]
mod tests {
    use super::native::{build_with_dir, parse_due_in, relative_due};
    use gtk::prelude::*;
    use gtk4 as gtk;

    #[test]
    fn due_offsets_parse_and_reject() {
        assert_eq!(parse_due_in("30m"), Some(1800));
        assert_eq!(parse_due_in("2h"), Some(7200));
        assert_eq!(parse_due_in("3d"), Some(259200));
        assert_eq!(parse_due_in("  1h  "), Some(3600));
        assert_eq!(parse_due_in(""), None);
        assert_eq!(parse_due_in("0h"), None);
        assert_eq!(parse_due_in("abc"), None);
        assert_eq!(parse_due_in("10x"), None);
        assert_eq!(parse_due_in("10"), None);
        assert_eq!(parse_due_in("-5m"), None);
        assert_eq!(parse_due_in("999999d"), None);
        // Non-ASCII input must fail closed, never panic on a char boundary.
        assert_eq!(parse_due_in("é"), None);
        assert_eq!(parse_due_in("30é"), None);
        assert_eq!(parse_due_in("1h🎉"), None);
        assert_eq!(parse_due_in("1hm"), None);
    }

    #[test]
    fn relative_labels_cover_past_and_future() {
        assert_eq!(relative_due(1000, 1000), "due now");
        assert_eq!(relative_due(1060, 1000), "in 1m");
        assert_eq!(relative_due(1000 + 3 * 3600 + 600, 1000), "in 3h 10m");
        assert_eq!(relative_due(1000 + 2 * 86400, 1000), "in 2d 0h");
        assert_eq!(relative_due(940, 1000), "1m overdue");
        assert_eq!(relative_due(700, 1000), "5m overdue");
        assert_eq!(relative_due(100000 - 9000, 100000), "2h overdue");
    }

    fn entry(root: &gtk::Widget, placeholder: &str) -> gtk::Entry {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Ok(entry) = widget.clone().downcast::<gtk::Entry>()
                && entry.placeholder_text().as_deref() == Some(placeholder)
            {
                return entry;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                stack.push(current.clone());
                child = current.next_sibling();
            }
        }
        panic!("entry {placeholder} not found")
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
    fn create_and_pause_round_trip_in_isolated_dir() {
        gtk::init().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "muse-tasks-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let page = build_with_dir(dir.clone(), None);
        let root: &gtk::Widget = page.root.upcast_ref();
        entry(root, "Title (1–200 chars)").set_text("Test reminder");
        entry(root, "Due in, e.g. 30m, 2h, 3d").set_text("1h");
        button(root, "Create reminder").emit_clicked();
        assert!(has_label(root, "created"));
        button(root, "Pause selected").emit_clicked();
        assert!(has_label(root, "Select a reminder"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
